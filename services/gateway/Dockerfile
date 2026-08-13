# syntax=docker/dockerfile:1.7
# See services/launcher/Dockerfile's own copy of this comment: compilation
# happens once, natively, in build-images.yml's build-workspace job, not
# in Docker -- this Dockerfile only packages the already-built binary.
#
# Pure Rust binary, no subprocess/shell use -- distroless: no shell, no
# package manager, nothing beyond libc and CA certs. This is the one
# JWT-authenticated process a compromised bearer token gets to talk to
# directly, so it also carries the least Kubernetes RBAC of anything in
# the stack -- ArmaServer read/write plus read-only pods/pods/log for
# GetServerHealth and GetServerLogs, still no Deployment access at all.
# See charts/magpie's RBAC templates.
#
# It also serves the web UI's static assets from /ui (see UI_DIR below),
# which is why this image has content beyond the binary.
FROM gcr.io/distroless/cc-debian13:nonroot

ENV NAMESPACE=default
ENV LISTEN_ADDR=0.0.0.0:8443
ENV SYNC_DAEMON_URL=http://sync-daemon:8080
# Where the web UI's built assets live in this image. Unset would mean
# "serve no UI" (see Config::ui_dir), which is the right behaviour for a
# binary built without the frontend but wrong for this image.
ENV UI_DIR=/ui

EXPOSE 8443

COPY --chmod=755 dist/gateway /gateway
# Built by Dockerfile.workspace's `web` stage into the same dist/
# artifact as the binaries.
COPY dist/ui /ui

ENTRYPOINT ["/gateway"]
