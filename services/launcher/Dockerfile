# syntax=docker/dockerfile:1.7
# Shared cargo-chef base for every service's Dockerfile in this repo.
# Replaces the old hand-maintained "stub every workspace member's src,
# build, delete the stubs" dance -- chef computes a dependency-only
# "recipe" from the workspace's actual Cargo.lock/manifests, so the
# expensive dependency-compile layer stays correctly cached across source
# changes without anyone having to keep a per-Dockerfile stub list in sync
# every time a crate is added to the workspace.
FROM rust:bookworm AS chef
RUN apt-get update \
    && apt-get install -y --no-install-recommends --no-install-suggests \
        protobuf-compiler \
        git \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
# No -p here, deliberately: cargo resolves a narrower, per-target feature
# set for `-p <crate>` than it does for a whole-workspace build under
# resolver v2, which doesn't match the feature set `cargo chef cook`
# (also whole-workspace, no -p) just built above -- that mismatch was
# silently invalidating and recompiling the *entire* dependency tree on
# every build even with a fully cache-hit cook layer. Matching cook's
# scope here means the extra cost is compiling the other services' own
# (small, workspace-owned) crates/binaries, not re-resolving the
# expensive third-party tree cook already cached.
RUN cargo build --release --locked \
    && cp /build/target/release/launcher /build/launcher

FROM debian:bookworm-slim

LABEL maintainer="Brett - github.com/brettmayson"
LABEL org.opencontainers.image.source=https://github.com/brettmayson/arma3server

SHELL ["/bin/bash", "-o", "pipefail", "-c"]
RUN apt-get update \
    && \
    apt-get install -y --no-install-recommends --no-install-suggests \
        lib32stdc++6 \
        lib32gcc-s1 \
        libcurl4 \
        ca-certificates \
        libstdc++6 \
        libssl3 \
        libc6 \
    && \
    apt-get remove --purge -y \
    && \
    apt-get clean autoclean \
    && \
    apt-get autoremove -y \
    && \
    rm -rf /var/lib/apt/lists/*

ENV ARMA_BINARY=./arma3server_x64
ENV ARMA_CONFIG=main.cfg
ENV ARMA_PARAMS=
ENV ARMA_PROFILE=main
ENV ARMA_WORLD=empty
ENV ARMA_LIMITFPS=1000
ENV ARMA_CDLC=
ENV HEADLESS_CLIENTS=0
ENV HEADLESS_CLIENTS_PROFILE="\$profile-hc-\$i"
ENV PORT=2302
ENV CLEAR_KEYS=true
ENV MODS=

EXPOSE 2302/udp
EXPOSE 2303/udp
EXPOSE 2304/udp
EXPOSE 2305/udp
EXPOSE 2306/udp

WORKDIR /arma3

VOLUME /arma3/server

STOPSIGNAL SIGINT

COPY --from=builder /build/launcher /launcher

CMD ["/launcher"]
