# syntax=docker/dockerfile:1.7
# The one Go service in an otherwise all-Rust services/ -- deliberate,
# not an oversight (see README.md's own "magpie-csi" bullet): Go has
# far deeper CSI-specific prior art (the official sidecars, most
# reference/community CSI drivers) than Rust does, and this is the most
# finicky piece of host-integration infra in the whole stack, so it's
# worth taking the well-travelled road here specifically. Doesn't
# participate in Dockerfile.workspace's shared Rust build for that same
# reason -- its own self-contained multi-stage build instead.
FROM golang:1.26-trixie AS build
WORKDIR /build
COPY services/magpie-csi/go.mod services/magpie-csi/go.sum ./
RUN go mod download
COPY services/magpie-csi/ ./
RUN CGO_ENABLED=0 go build -o /magpie-csi .

# Not distroless -- like the service it replaced (volume-manager), this
# shells out to real host tools (losetup/mkfs.btrfs/mount/umount/
# findmnt/df) rather than binding raw ioctls directly, and needs the
# toolset installed to do anything useful at all.
FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends --no-install-suggests \
        btrfs-progs \
        util-linux \
        coreutils \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build --chmod=755 /magpie-csi /magpie-csi

CMD ["/magpie-csi"]
