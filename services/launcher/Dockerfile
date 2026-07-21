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

# arma3server_x64 shells out via system()/popen() for a handful of real
# userland tools (confirmed live, one at a time, by the exact "sh: 1:
# <cmd>: not found"/"<cmd>: not found" error format only a real shell
# produces: /bin/sh itself, ifconfig, then later cat/grep/sed/uniq once
# the first crop was fixed and the server got further into startup) --
# even though this launcher's own Rust code never shells out to anything
# (see launch.rs's own doc: it spawns arma3server_x64 directly). Rather
# than chasing each individually-missing tool as it surfaces (the
# dash-only approach this replaced), busybox-static is one single fully
# static binary (confirmed via ldd: "not a dynamic executable", zero
# runtime deps at all, not even libc) providing every POSIX-ish tool
# arma3server_x64 could plausibly still shell out to, applet-dispatched
# by argv[0]/symlink name -- covers whatever surfaces next too, not just
# what's been observed so far.
FROM debian:trixie-slim AS tools
RUN apt-get update \
    && apt-get install -y --no-install-recommends --no-install-suggests \
        busybox-static \
    && rm -rf /var/lib/apt/lists/*
# Staged under /out, not installed into this stage's own /usr/bin
# directly -- debian:trixie-slim already ships real cat/grep/sed/uniq
# there as essential packages (confirmed live), so `busybox --install`
# against the real /usr/bin would skip every name that already exists
# and leave those as dynamically-linked Debian binaries with their own
# extra .so deps (libpcre2/libacl/libselinux for grep/sed) distroless/cc
# doesn't have -- copying this stage's whole /usr/bin wholesale would
# also drag in every *other* Debian userland binary along with it,
# defeating the point of a minimal image. Building a clean tree from
# scratch and copying only that instead avoids both problems.
RUN mkdir -p /out/usr/bin /out/bin /out/sbin \
    && cp /usr/bin/busybox /out/usr/bin/busybox \
    && /out/usr/bin/busybox --install -s /out/usr/bin \
    && ln -s /usr/bin/busybox /out/bin/sh \
    && ln -s /usr/bin/busybox /out/bin/cat \
    && ln -s /usr/bin/busybox /out/sbin/ifconfig

# distroless:nonroot, like every other service in this chart -- unlike
# them, this needed a real userland Dockerfile until very recently
# (net-tools/lib32*/libcurl4/libssl3, all pulled in for arma3server_x64
# itself, not this launcher's own Rust code) purely because nobody had
# gone through and confirmed exactly what was actually load-bearing vs.
# carried over from the community image this was ported from. Confirmed
# live via ldd against the actual synced binaries: arma3server_x64/
# libsteam_api.so/steamclient.so need only libpthread/librt/libdl/libm/
# libgcc_s/libc -- all already in distroless/cc -- plus busybox above
# (statically linked, so it needs none of those either). The real,
# separate bug that made this launcher *seem* to need root all along
# was actually a missing /etc/passwd entry for the fixed nonroot UID
# (confirmed live: SteamAPI_Init() crashes outright, not gracefully,
# when getpwuid() has nothing to return) -- distroless:nonroot's own
# baked-in nonroot user fixes that for free, which is the actual reason
# this now works non-root at all, not anything about capabilities or
# root's own DAC bypass.
FROM gcr.io/distroless/cc-debian13:nonroot

# The real busybox binary plus every applet name (grep/sed/uniq/etc.)
# busybox --install symlinked to it, staged under /out in the tools
# stage -- copied wholesale, not enumerated one at a time, so whatever
# arma3server_x64 shells out to next that hasn't surfaced yet is
# already covered instead of needing another round of this same fix.
COPY --from=tools /out/usr/bin/ /usr/bin/
COPY --from=tools /out/bin/ /bin/
COPY --from=tools /out/sbin/ /sbin/
ENV PATH=/usr/bin:/bin:/sbin

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
