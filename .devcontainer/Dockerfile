# See here for image contents: https://github.com/microsoft/vscode-dev-containers/tree/v0.233.0/containers/go/.devcontainer/base.Dockerfile

# [Choice] Go version (use -bullseye variants on local arm64/Apple Silicon): 1, 1.16, 1.17, 1-bullseye, 1.16-bullseye, 1.17-bullseye, 1-buster, 1.16-buster, 1.17-buster

FROM mcr.microsoft.com/devcontainers/base:jammy

RUN apt-get update && apt-get upgrade && export DEBIAN_FRONTEND=noninteractive \
  && apt-get -y install --no-install-recommends \
  # Chromium for running Turbopack benchmarks
  chromium \
  # Used for plotters graph visualizations in turbopack benchmarks
  libfontconfig1-dev \
  golang-goprotobuf-dev \
  protobuf-compiler \
  capnproto

RUN go install google.golang.org/protobuf/cmd/protoc-gen-go@v1.28.0 && go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@v1.2.0