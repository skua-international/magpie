# sync-daemon resync benchmarking

Tools used to measure sync-daemon's resource usage/throughput across a full
golden-content resync on elephant. Assumes the `elephant-kubeconfig`
(`$KUBECONFIG`, defaults to `/home/grim/.kube/config-205-209-101-194`) and
`magpiectl` are available.

## `watch-sync-daemon.sh`

Polls `kubectl top pod`/`du` on the CSI blob/`/proc/net/dev` every
`INTERVAL` seconds (default 10s) and appends a CSV row
(`sync-daemon-usage-<timestamp>.csv` in the current directory) with CPU,
memory, blob size, throughput, and (if `TARGET_TOTAL_KIB` is set)
progress/ETA. `AUTO_STOP=1` makes it exit on its own once blob growth goes
idle for `IDLE_POLLS` consecutive polls.

Standalone-usable for watching an already-running sync, or driven by
`benchmark-deploy.sh` below.

## `benchmark-deploy.sh`

Nukes the `workshop/` subtree of the golden-content PVC and triggers a full
resync, then hands off to `watch-sync-daemon.sh` with `AUTO_STOP=1`.

Two modes:
- **Default**: refreshes Steam auth (`magpiectl admin refresh-steam-auth`,
  interactive QR scan) then scales sync-daemon to 0, nukes via a debug pod
  mounting the PVC directly, scales back to 1 (triggering a fresh
  startup sync). `SKIP_AUTH=1` skips the QR step and reuses whatever
  session is already stored.
- **`NO_RESTART=1`**: skips the whole auth dance -- attaches an ephemeral
  debug container to the *running* pod (shares its volumes, no PVC
  re-mount/RWO conflict) to do the nuke, then triggers a sync via
  `magpiectl mods sync <id>` for every registered mod source instead of
  relying on a restart's own startup sync. Needs a kubectl client new
  enough for `kubectl debug --custom` (1.30+).

`TARGET_GIB` overrides the pre-nuke size measurement (useful when the last
run was interrupted and the live measurement would be stale/partial).

Known open issue, not filed yet: firing many `mods sync` calls in
`NO_RESTART=1` mode close together can hit a turso `concurrent use
forbidden` error on `upsert_source` (a plain `.execute()` call, so *not*
the query-iteration Mutex-guard bug fixed in `cache.rs` -- that one only
affected row-iterating `.query()` methods). Root cause still unconfirmed;
possibly a cancellation-safety issue if a timeout/retry wrapper drops an
in-flight turso operation mid-transaction. The script tolerates a single
failure per source and stays unblocked, but the underlying race is worth
investigating if it recurs.
