use std::{io::Write, sync::Arc, time::Duration};

use console::StyledObject;
use tracing::{debug, log::warn};
use turbopath::{AbsoluteSystemPath, AbsoluteSystemPathBuf, AnchoredSystemPathBuf};
use turborepo_cache::{AsyncCache, CacheError, CacheResponse, CacheSource};
use turborepo_ui::{
    color, replay_logs, ColorSelector, LogWriter, PrefixedUI, PrefixedWriter, GREY, UI,
};

use crate::{
    cli::OutputLogsMode,
    daemon::{DaemonClient, DaemonConnector},
    opts::RunCacheOpts,
    package_graph::WorkspaceInfo,
    run::task_id::TaskId,
    task_graph::{TaskDefinition, TaskOutputs},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Error replaying logs: {0}")]
    Ui(#[from] turborepo_ui::Error),
    #[error("Error accessing cache: {0}")]
    Cache(#[from] turborepo_cache::CacheError),
    #[error("Error finding outputs to save: {0}")]
    Globwalk(#[from] globwalk::WalkError),
    #[error("Error with daemon: {0}")]
    Daemon(#[from] crate::daemon::DaemonError),
    #[error("no connection to daemon")]
    NoDaemon,
}

impl Error {
    pub fn is_cache_miss(&self) -> bool {
        matches!(&self, Self::Cache(CacheError::CacheMiss))
    }
}

pub struct RunCache {
    task_output_mode: Option<OutputLogsMode>,
    cache: AsyncCache,
    reads_disabled: bool,
    writes_disabled: bool,
    repo_root: AbsoluteSystemPathBuf,
    color_selector: ColorSelector,
    daemon_client: Option<DaemonClient<DaemonConnector>>,
    ui: UI,
}

impl RunCache {
    pub fn new(
        cache: AsyncCache,
        repo_root: &AbsoluteSystemPath,
        opts: &RunCacheOpts,
        color_selector: ColorSelector,
        daemon_client: Option<DaemonClient<DaemonConnector>>,
        ui: UI,
    ) -> Self {
        RunCache {
            task_output_mode: opts.task_output_mode_override,
            cache,
            reads_disabled: opts.skip_reads,
            writes_disabled: opts.skip_writes,
            repo_root: repo_root.to_owned(),
            color_selector,
            daemon_client,
            ui,
        }
    }

    pub fn task_cache(
        self: &Arc<Self>,
        // TODO: Group these in a struct
        task_definition: &TaskDefinition,
        workspace_info: &WorkspaceInfo,
        task_id: TaskId<'static>,
        hash: &str,
    ) -> TaskCache {
        let log_file_path = self
            .repo_root
            .resolve(workspace_info.package_path())
            .resolve(&TaskDefinition::workspace_relative_log_file(task_id.task()));
        let repo_relative_globs =
            task_definition.repo_relative_hashable_outputs(&task_id, workspace_info.package_path());

        let mut task_output_mode = task_definition.output_mode;
        if let Some(task_output_mode_override) = self.task_output_mode {
            task_output_mode = task_output_mode_override;
        }

        let caching_disabled = !task_definition.cache;

        TaskCache {
            expanded_outputs: Vec::new(),
            run_cache: self.clone(),
            repo_relative_globs,
            hash: hash.to_owned(),
            task_id,
            task_output_mode,
            caching_disabled,
            log_file_path,
            daemon_client: self.daemon_client.clone(),
            ui: self.ui,
        }
    }
}

pub struct TaskCache {
    expanded_outputs: Vec<AnchoredSystemPathBuf>,
    run_cache: Arc<RunCache>,
    repo_relative_globs: TaskOutputs,
    hash: String,
    task_output_mode: OutputLogsMode,
    caching_disabled: bool,
    log_file_path: AbsoluteSystemPathBuf,
    daemon_client: Option<DaemonClient<DaemonConnector>>,
    ui: UI,
    task_id: TaskId<'static>,
}

impl TaskCache {
    pub fn replay_log_file(&self, prefixed_ui: &mut PrefixedUI<impl Write>) -> Result<(), Error> {
        if self.log_file_path.exists() {
            replay_logs(prefixed_ui, &self.log_file_path)?;
        }

        Ok(())
    }

    pub fn on_error(&self, prefixed_ui: &mut PrefixedUI<impl Write>) -> Result<(), Error> {
        if self.task_output_mode == OutputLogsMode::ErrorsOnly {
            prefixed_ui.output(format!(
                "cache miss, executing {}",
                color!(self.ui, GREY, "{}", self.hash)
            ));
            self.replay_log_file(prefixed_ui)?;
        }

        Ok(())
    }

    pub fn output_writer<W: Write>(
        &self,
        prefix: StyledObject<String>,
        writer: W,
    ) -> Result<LogWriter<W>, Error> {
        let mut log_writer = LogWriter::default();
        let prefixed_writer = PrefixedWriter::new(self.run_cache.ui, prefix, writer);

        if self.caching_disabled || self.run_cache.writes_disabled {
            log_writer.with_prefixed_writer(prefixed_writer);
            return Ok(log_writer);
        }

        log_writer.with_log_file(&self.log_file_path)?;

        if matches!(
            self.task_output_mode,
            OutputLogsMode::Full | OutputLogsMode::NewOnly
        ) {
            log_writer.with_prefixed_writer(prefixed_writer);
        }

        Ok(log_writer)
    }

    pub async fn restore_outputs(
        &mut self,
        prefixed_ui: &mut PrefixedUI<impl Write>,
    ) -> Result<CacheResponse, Error> {
        if self.caching_disabled || self.run_cache.reads_disabled {
            if !matches!(
                self.task_output_mode,
                OutputLogsMode::None | OutputLogsMode::ErrorsOnly
            ) {
                prefixed_ui.output(format!(
                    "cache bypass, force executing {}",
                    color!(self.ui, GREY, "{}", self.hash)
                ));
            }

            return Err(CacheError::CacheMiss.into());
        }

        let changed_output_count = if let Some(daemon_client) = &mut self.daemon_client {
            match daemon_client
                .get_changed_outputs(
                    self.hash.to_string(),
                    self.repo_relative_globs.inclusions.clone(),
                )
                .await
            {
                Ok(changed_output_globs) => changed_output_globs.len(),
                Err(err) => {
                    warn!(
                        "Failed to check if we can skip restoring outputs for {}: {:?}. \
                         Proceeding to check cache",
                        self.task_id, err
                    );
                    self.repo_relative_globs.inclusions.len()
                }
            }
        } else {
            self.repo_relative_globs.inclusions.len()
        };

        let has_changed_outputs = changed_output_count > 0;

        let cache_status = if has_changed_outputs {
            // Note that we currently don't use the output globs when restoring, but we
            // could in the future to avoid doing unnecessary file I/O. We also
            // need to pass along the exclusion globs as well.
            let (cache_status, restored_files) = self
                .run_cache
                .cache
                .fetch(&self.run_cache.repo_root, &self.hash)
                .await
                .map_err(|err| {
                    if matches!(err, CacheError::CacheMiss) {
                        prefixed_ui.output(format!(
                            "cache miss, executing {}",
                            color!(self.ui, GREY, "{}", self.hash)
                        ));
                    }

                    err
                })?;

            self.expanded_outputs = restored_files;

            if let Some(daemon_client) = &mut self.daemon_client {
                if let Err(err) = daemon_client
                    .notify_outputs_written(
                        self.hash.clone(),
                        self.repo_relative_globs.inclusions.clone(),
                        self.repo_relative_globs.exclusions.clone(),
                        cache_status.time_saved,
                    )
                    .await
                {
                    // Don't fail the whole operation just because we failed to
                    // watch the outputs
                    prefixed_ui.warn(color!(
                        self.ui,
                        GREY,
                        "Failed to mark outputs as cached for {}: {:?}",
                        self.task_id,
                        err
                    ))
                }
            }

            cache_status
        } else {
            CacheResponse {
                source: CacheSource::Local,
                time_saved: 0,
            }
        };

        let more_context = if has_changed_outputs {
            ""
        } else {
            " (outputs already on disk)"
        };

        match self.task_output_mode {
            OutputLogsMode::HashOnly => {
                prefixed_ui.output(format!(
                    "cache hit{}, suppressing logs {}",
                    more_context,
                    color!(self.ui, GREY, "{}", self.hash)
                ));
            }
            OutputLogsMode::Full => {
                debug!("log file path: {}", self.log_file_path);
                prefixed_ui.output(format!(
                    "cache hit{}, replaying logs {}",
                    more_context,
                    color!(self.ui, GREY, "{}", self.hash)
                ));
                self.replay_log_file(prefixed_ui)?;
            }
            _ => {}
        }

        Ok(cache_status)
    }

    pub async fn save_outputs(
        &mut self,
        prefixed_ui: &mut PrefixedUI<impl Write>,
        duration: Duration,
    ) -> Result<(), Error> {
        if self.caching_disabled || self.run_cache.reads_disabled {
            return Ok(());
        }

        debug!("caching outputs: outputs: {:?}", &self.repo_relative_globs);

        let files_to_be_cached = globwalk::globwalk(
            &self.run_cache.repo_root,
            &self.repo_relative_globs.inclusions,
            &self.repo_relative_globs.exclusions,
            globwalk::WalkType::All,
        )?;

        let mut relative_paths = files_to_be_cached
            .into_iter()
            .map(|path| {
                AnchoredSystemPathBuf::relative_path_between(&self.run_cache.repo_root, &path)
            })
            .collect::<Vec<_>>();
        relative_paths.sort();
        self.run_cache
            .cache
            .put(
                self.run_cache.repo_root.clone(),
                self.hash.clone(),
                relative_paths.clone(),
                duration.as_millis() as u64,
            )
            .await?;

        if let Some(daemon_client) = self.daemon_client.as_mut() {
            let notify_result = daemon_client
                .notify_outputs_written(
                    self.hash.to_string(),
                    self.repo_relative_globs.inclusions.clone(),
                    self.repo_relative_globs.exclusions.clone(),
                    duration.as_millis() as u64,
                )
                .await
                .map_err(Error::from);

            if let Err(err) = notify_result {
                let task_id = &self.task_id;
                warn!("Failed to mark outputs as cached for {task_id}: {err}");
                prefixed_ui.warn(format!(
                    "Failed to mark outputs as cached for {task_id}: {err}",
                ));
            }
        }

        self.expanded_outputs = relative_paths;

        Ok(())
    }

    pub fn expanded_outputs(&self) -> &[AnchoredSystemPathBuf] {
        &self.expanded_outputs
    }
}
