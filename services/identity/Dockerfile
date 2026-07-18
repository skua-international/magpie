# syntax=docker/dockerfile:1.7
FROM rust:bookworm AS chef
RUN apt-get update \
    && apt-get install -y --no-install-recommends --no-install-suggests \
        protobuf-compiler \
        git \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo install cargo-chef --locked
WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json --bin identity

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo chef cook --release --recipe-path recipe.json -p identity
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p identity \
    && cp /build/target/release/identity /build/identity

# Pure Rust binary, no subprocess/shell use -- distroless: no shell, no
# package manager, nothing beyond libc and CA certs. This process holds
# the cluster's only signing key and every OAuth2 client secret -- keeping
# its runtime surface minimal matters more here than almost anywhere else
# in the stack.
FROM gcr.io/distroless/cc-debian12:nonroot

ENV LISTEN_ADDR=0.0.0.0:8445

EXPOSE 8445

COPY --from=builder /build/identity /identity

ENTRYPOINT ["/identity"]
