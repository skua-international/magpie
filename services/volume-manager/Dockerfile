# syntax=docker/dockerfile:1.7
# See services/launcher/Dockerfile's own copy of this comment: compilation
# happens once, natively, in build-images.yml's build-workspace job, not
# in Docker -- this Dockerfile only packages the already-built binary.
#
# Unlike every other service's Dockerfile, this one isn't distroless/slim
# -- it shells out to truncate/losetup/mkfs.btrfs/btrfs/mount/findmnt/df/
# nsenter at runtime (see src/blob.rs), which need CAP_SYS_ADMIN and
# hostPID: true (not privileged: true -- see the chart's DaemonSet and
# blob.rs's own doc for why that's no longer required) to do anything
# useful at all. Keeping this the *only* image in the stack with that
# toolset installed matters as much as the capability grant itself --
# every other service's image genuinely cannot run these commands even
# if somehow granted the same privilege. nsenter itself ships as part of
# util-linux, already installed below.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends --no-install-suggests \
        btrfs-progs \
        util-linux \
        coreutils \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ENV LISTEN_ADDR=0.0.0.0:8446
ENV BLOB_IMAGE_PATH=/blob/content.img
ENV BLOB_MOUNT_PATH=/mnt/blob

EXPOSE 8446

COPY --chmod=755 dist/volume-manager /volume-manager

CMD ["/volume-manager"]
