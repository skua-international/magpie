# syntax=docker/dockerfile:1.7
# See services/launcher/Dockerfile's own copy of this comment: compilation
# happens once, natively, in build-images.yml's build-workspace job, not
# in Docker -- this Dockerfile only packages the already-built binary.
#
# Pure Rust binary, no subprocess/shell use -- distroless: no shell, no
# package manager, nothing beyond libc and CA certs. This is the one
# JWT-authenticated process a compromised bearer token gets to talk to
# directly, so it also carries the least Kubernetes RBAC of anything in
# the stack (ArmaServer read/write only, no Deployment access) -- see
# charts/magpie's RBAC templates.
FROM gcr.io/distroless/cc-debian12:nonroot

ENV NAMESPACE=default
ENV LISTEN_ADDR=0.0.0.0:8443
ENV SYNC_DAEMON_URL=http://sync-daemon:8080

EXPOSE 8443

COPY --chmod=755 dist/server-api /server-api

ENTRYPOINT ["/server-api"]
