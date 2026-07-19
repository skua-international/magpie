# syntax=docker/dockerfile:1.7
# Compilation happens once, natively, in build-images.yml's build-workspace
# job (Swatinem/rust-cache, no Docker involved) -- this Dockerfile only
# packages the already-built binary. Was a cargo-chef multi-stage build
# until this repo hit two separate real caching bugs from it in one
# session: a -p-scoping mismatch between `cook` and the final build that
# silently defeated the cache even on a cook hit, and a cache-scope race
# across the 6 services' concurrent matrix builds that served one
# service's planner stage a stale recipe.json from a different commit.
# Splitting "compile" from "package" into separate jobs removes both
# failure classes outright instead of patching around them.
FROM debian:trixie-slim

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

COPY --chmod=755 dist/launcher /launcher

CMD ["/launcher"]
