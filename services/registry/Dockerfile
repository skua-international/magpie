# syntax=docker/dockerfile:1.7
# See services/launcher/Dockerfile's own copy of this comment: compilation
# happens once, natively, in build-images.yml's build-workspace job, not
# in Docker -- this Dockerfile only packages the already-built binary.
#
# Pure Rust binary, no subprocess/shell use -- distroless: no shell, no
# package manager, nothing beyond libc and CA certs.
FROM gcr.io/distroless/cc-debian12:nonroot

ENV LISTEN_ADDR=0.0.0.0:8444
ENV LOCAL_CONTENT_ROOT=/local-content
ENV SYNC_DAEMON_URL=http://sync-daemon:8080

EXPOSE 8444

COPY --chmod=755 dist/registry /registry

ENTRYPOINT ["/registry"]
