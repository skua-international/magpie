# syntax=docker/dockerfile:1.7
# See services/launcher/Dockerfile's own copy of this comment: compilation
# happens once, natively, in build-images.yml's build-workspace job, not
# in Docker -- this Dockerfile only packages the already-built binary.
#
# Pure Rust binary, no subprocess/shell use -- distroless: no shell, no
# package manager, nothing beyond libc and CA certs. This process holds
# the cluster's only signing key and every OAuth2 client secret -- keeping
# its runtime surface minimal matters more here than almost anywhere else
# in the stack.
FROM gcr.io/distroless/cc-debian13:nonroot

ENV LISTEN_ADDR=0.0.0.0:8445

EXPOSE 8445

COPY --chmod=755 dist/identity /identity

ENTRYPOINT ["/identity"]
