# syntax=docker/dockerfile:1.7
# Compilation happens once, natively, in build-images.yml's build-workspace
# job (Swatinem/rust-cache, no Docker involved) -- this Dockerfile only
# packages the already-built binary. Was a cargo-chef multi-stage build
# until this repo hit two separate real caching bugs from it in one
# session: a -p-scoping mismatch between `cook` and the final build that
# silently defeated the cache even on a cook hit, and a cache-scope race
# across the 6 services' concurrent matrix builds that served one
# service's planner stage a stale recipe.json from a different commit.
# Splitting "compile" from "package" into separate jobs removes both
# failure classes outright instead of patching around them.

# Two binaries borrowed from a real Debian userland, not this image's own
# package manager (distroless has none) -- both confirmed to need nothing
# beyond libc.so.6/ld-linux, which distroless/cc already provides, so
# copying them in adds no further runtime dependencies at all:
#   - /bin/dash as /bin/sh: arma3server_x64 shells out via system()/
#     popen() (confirmed live -- the exact "sh: 1: <cmd>: not found"
#     error format only a real shell produces) for at least ifconfig
#     below, so *some* /bin/sh has to exist even though this launcher's
#     own Rust code never uses one (see launch.rs's own doc: it spawns
#     arma3server_x64 directly, no shell, specifically so that isn't
#     needed on this side).
#   - /sbin/ifconfig: arma3server_x64 shells out to it directly at
#     startup (confirmed live: "sh: 1: /sbin/ifconfig: not found"
#     before this existed) -- the old net-tools package pulled in a lot
#     more than just this one binary for it.
FROM debian:trixie-slim AS tools
RUN apt-get update \
    && apt-get install -y --no-install-recommends --no-install-suggests \
        net-tools \
    && rm -rf /var/lib/apt/lists/*

# distroless:nonroot, like every other service in this chart -- unlike
# them, this needed a real userland Dockerfile until very recently
# (net-tools/lib32*/libcurl4/libssl3, all pulled in for arma3server_x64
# itself, not this launcher's own Rust code) purely because nobody had
# gone through and confirmed exactly what was actually load-bearing vs.
# carried over from the community image this was ported from. Confirmed
# live via ldd against the actual synced binaries: arma3server_x64/
# libsteam_api.so/steamclient.so need only libpthread/librt/libdl/libm/
# libgcc_s/libc -- all already in distroless/cc -- plus the two tools
# copied in above. The real, separate bug that made this launcher
# *seem* to need root all along was actually a missing /etc/passwd
# entry for the fixed nonroot UID (confirmed live: SteamAPI_Init()
# crashes outright, not gracefully, when getpwuid() has nothing to
# return) -- distroless:nonroot's own baked-in nonroot user fixes that
# for free, which is the actual reason this now works non-root at all,
# not anything about capabilities or root's own DAC bypass.
FROM gcr.io/distroless/cc-debian13:nonroot

COPY --from=tools /bin/dash /bin/sh
COPY --from=tools /sbin/ifconfig /sbin/ifconfig

ENV ARMA_BINARY=./arma3server_x64
ENV ARMA_CONFIG=main.cfg
ENV ARMA_PARAMS=
ENV ARMA_PROFILE=main
ENV ARMA_WORLD=empty
ENV ARMA_LIMITFPS=1000
ENV ARMA_CDLC=
ENV PORT=2302
ENV CLEAR_KEYS=true
ENV MODS=

EXPOSE 2302/udp
EXPOSE 2303/udp
EXPOSE 2304/udp
EXPOSE 2305/udp
EXPOSE 2306/udp

WORKDIR /arma3

VOLUME /arma3/server

STOPSIGNAL SIGINT

COPY --chmod=755 dist/launcher /launcher

ENTRYPOINT ["/launcher"]
