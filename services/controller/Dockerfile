# syntax=docker/dockerfile:1.7
# See services/launcher/Dockerfile's own copy of this comment: compilation
# happens once, natively, in build-images.yml's build-workspace job, not
# in Docker -- this Dockerfile only packages the already-built binary.
#
# Pure Rust binary, no subprocess/shell use at all (unlike sync-daemon,
# which shells out to `cp` for reflink claims) -- distroless gets this one
# no shell, no package manager, nothing beyond libc and CA certs.
FROM gcr.io/distroless/cc-debian12:nonroot

ENV NAMESPACE=default
ENV LAUNCHER_IMAGE=arma3-launcher:latest
ENV SYNC_DAEMON_URL=http://sync-daemon:8080
ENV CLAIMS_HOST_PATH=/var/lib/magpie/claims
ENV CLAIMS_ROOT=/claims
ENV SERVER_ROOT_BASE=/srv/arma-servers
ENV LOCAL_CONTENT_ROOT=/local-content
ENV LOCAL_CONTENT_HOST_PATH=/var/lib/magpie/local-content

COPY --chmod=755 dist/controller /controller

ENTRYPOINT ["/controller"]
