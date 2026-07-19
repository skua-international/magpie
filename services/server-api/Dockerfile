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
    && cp /build/target/release/server-api /build/server-api

# Pure Rust binary, no subprocess/shell use -- distroless: no shell, no
# package manager, nothing beyond libc and CA certs. This is the one
# JWT-authenticated process a compromised bearer token gets to talk to
# directly, so it also carries the least Kubernetes RBAC of anything in
# the stack (ArmaServer read/write only, no Deployment access) -- see
# charts/magpie's RBAC templates.
FROM gcr.io/distroless/cc-debian12:nonroot

ENV NAMESPACE=default
ENV LISTEN_ADDR=0.0.0.0:8443
ENV SYNC_DAEMON_URL=http://sync-daemon:8080

EXPOSE 8443

COPY --from=builder /build/server-api /server-api

ENTRYPOINT ["/server-api"]
