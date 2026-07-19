# syntax=docker/dockerfile:1.7
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
# No -p here, deliberately -- see services/launcher/Dockerfile's own copy
# of this comment: a -p-scoped build resolves a narrower feature set than
# cook's whole-workspace one, defeating the cache even on a cook hit.
RUN cargo build --release --locked \
    && cp /build/target/release/controller /build/controller

# Pure Rust binary, no subprocess/shell use at all (unlike sync-daemon,
# which shells out to `cp` for reflink claims) -- distroless gets this one
# no shell, no package manager, nothing beyond libc and CA certs.
FROM gcr.io/distroless/cc-debian12:nonroot

ENV NAMESPACE=default
ENV LAUNCHER_IMAGE=arma3-launcher:latest
ENV SYNC_DAEMON_URL=http://sync-daemon:8080
ENV CLAIMS_HOST_PATH=/var/lib/magpie/claims
ENV CLAIMS_ROOT=/claims
ENV SERVER_ROOT_BASE=/srv/arma-servers
ENV LOCAL_CONTENT_ROOT=/local-content
ENV LOCAL_CONTENT_HOST_PATH=/var/lib/magpie/local-content

COPY --from=builder /build/controller /controller

ENTRYPOINT ["/controller"]
