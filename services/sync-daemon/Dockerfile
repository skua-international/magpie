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
RUN cargo chef prepare --recipe-path recipe.json --bin sync-daemon

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p sync-daemon
COPY . .
RUN cargo build --release --locked -p sync-daemon

FROM debian:bookworm-slim

# Needs a real userland, not distroless: steam_sync::claim shells out to
# `cp --reflink=always` (see crates/steam-sync/src/claim.rs) rather than
# binding the platform-specific FICLONE ioctl directly. `cp` (coreutils) is
# already part of debian-slim's base install -- nothing extra needed here.
RUN apt-get update \
    && apt-get install -y --no-install-recommends --no-install-suggests \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ENV CONTENT_ROOT=/content
ENV CLAIMS_ROOT=/claims
ENV LISTEN_ADDR=0.0.0.0:8080
ENV POOL_SIZE=4

EXPOSE 8080

VOLUME /content
VOLUME /claims

COPY --from=builder /build/target/release/sync-daemon /sync-daemon

CMD ["/sync-daemon"]
