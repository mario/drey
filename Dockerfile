# syntax=docker/dockerfile:1.19
#
# Build and test only. There is no runtime image here on purpose.
#
# drey is a per-user local daemon: it shares a Unix socket, a filesystem and a
# process lifetime with your editor. A container gets you none of those, and a
# published runtime image would be an invitation into a setup that cannot work
# well. Install with `cargo install drey` or the Homebrew tap instead.
#
# What this is good for: reproducing the Linux build from a Mac, and giving a
# contributor one command that runs everything CI runs.
#
#   docker build --target verify .
#
# Every base image is pinned by digest, never by a tag alone. A tag is a
# mutable pointer; a digest is the image you actually tested against.

FROM rust:1.96.0-bookworm@sha256:5e2214abe154fe26e39f64488952e5c991eeed1d6d6da7cc8381ae83927f0cfc AS base
WORKDIR /src
# The end-to-end suite drives the shim from Python, so the toolchain needs it.
RUN apt-get update \
    && apt-get install --no-install-recommends --yes python3 \
    && rm -rf /var/lib/apt/lists/*
RUN rustup component add rustfmt clippy

FROM base AS verify
COPY . .
RUN cargo fmt --all -- --check
RUN cargo clippy --all-targets -- -D warnings
RUN cargo test
# e2e needs a built binary, and finds it through cargo metadata.
RUN cargo build && python3 tests/e2e.py

# `docker build --target msrv .` proves the crate still builds on the oldest
# Rust it claims to support, which is easy to break without noticing.
FROM rust:1.85.0-bookworm@sha256:0ff31c9ffa641a62e48d543fb00b4960955ea375f40776f40f585b89e654cc5e AS msrv
WORKDIR /src
COPY . .
RUN cargo check --all-targets
