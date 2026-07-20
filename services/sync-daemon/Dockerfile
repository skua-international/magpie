# syntax=docker/dockerfile:1.7
# See services/launcher/Dockerfile's own copy of this comment: compilation
# happens once, natively, in build-images.yml's build-workspace job, not
# in Docker -- this Dockerfile only packages the already-built binary.
FROM debian:trixie-slim

# Needs a real userland, not distroless: steam_sync::claim shells out to
# `btrfs subvolume snapshot`/`delete` (see crates/steam-sync/src/claim.rs)
# rather than binding the ioctls directly, so the `btrfs` CLI (btrfs-progs)
# has to actually be present in the image -- it isn't part of any Debian
# base install.
RUN apt-get update \
    && apt-get install -y --no-install-recommends --no-install-suggests \
        ca-certificates \
        btrfs-progs \
    && rm -rf /var/lib/apt/lists/*

ENV CONTENT_ROOT=/content
ENV CLAIMS_ROOT=/claims
ENV LISTEN_ADDR=0.0.0.0:8080
ENV POOL_SIZE=4

EXPOSE 8080

VOLUME /content
VOLUME /claims

COPY --chmod=755 dist/sync-daemon /sync-daemon

CMD ["/sync-daemon"]
