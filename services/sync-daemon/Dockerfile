# syntax=docker/dockerfile:1.7
# See services/launcher/Dockerfile's own copy of this comment: compilation
# happens once, natively, in build-images.yml's build-workspace job, not
# in Docker -- this Dockerfile only packages the already-built binary.
#
# Pure Rust binary, no subprocess/shell use at all any more -- the btrfs
# subvolume snapshot/delete work per ArmaServer moved entirely into
# services/magpie-csi's own driver (a CSI inline ephemeral volume, not
# something this process touches or even knows about), so this can go
# distroless like everything else now: no shell, no package manager,
# nothing beyond libc and CA certs (for TLS to Steam's own CDN/CM
# servers).
FROM gcr.io/distroless/cc-debian13:nonroot

ENV CONTENT_ROOT=/content
ENV LISTEN_ADDR=0.0.0.0:8080
ENV POOL_SIZE=4

EXPOSE 8080

VOLUME /content

COPY --chmod=755 dist/sync-daemon /sync-daemon

ENTRYPOINT ["/sync-daemon"]
