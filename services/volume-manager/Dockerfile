# syntax=docker/dockerfile:1.7
# See services/launcher/Dockerfile's own copy of this comment: compilation
# happens once, natively, in build-images.yml's build-workspace job, not
# in Docker -- this Dockerfile only packages the already-built binary.
#
# Unlike every other service's Dockerfile, this one isn't distroless/slim
# -- it shells out to truncate/losetup/mkfs.btrfs/btrfs/mount/findmnt/df
# at runtime (see src/blob.rs), all of which need real host block-device
# privilege (securityContext.privileged: true in the chart's DaemonSet)
# to do anything useful at all. Keeping this the *only* image in the
# stack with that toolset installed matters as much as the privileged
# securityContext itself -- every other service's image genuinely cannot
# run these commands even if somehow granted the capability to.
FROM debian:bookworm-slim

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
