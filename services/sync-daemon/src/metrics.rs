//! sync-daemon's own metrics.
//!
//! These exist because answering "how loaded is sync-daemon" or "how fast
//! is this resync going" previously meant shelling into the magpie-csi
//! node pod to `du` the loop-backed blob file and diffing samples by hand
//! (see issue #43's own comment, written during a full golden-content
//! resync load test).
//!
//! Deliberately *not* here: process CPU, memory and restart counts. Those
//! come free from cAdvisor and kube-state-metrics via the pod labels that
//! already exist, and duplicating them in-process is the thing the commit
//! that introduced magpie's metrics explicitly chose not to do. What
//! follows is only what nothing else can see.

use std::sync::Arc;
use std::time::Duration;

use crate::service::Shared;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;

/// How often the size gauges are refreshed. Matches the other services'
/// poll interval; content size moves on the scale of a sync, not a
/// second.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

// Download throughput and per-chunk timing are deliberately absent for
// now. Both need the counts that only exist inside steam-sync's own
// chunk loop and progress callback, which would mean threading a meter
// through that crate -- worth doing, but as its own change rather than
// shipping instruments here that would always read zero. Content size
// below already makes a resync's progress visible, which was the
// immediate gap.

/// Wall-clock duration of one depot or mod sync.
///
/// Seconds, because that is the OpenTelemetry convention and what
/// Prometheus histogram tooling assumes.
pub fn sync_duration() -> &'static Histogram<f64> {
    static H: std::sync::OnceLock<Histogram<f64>> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        observability::meter()
            .f64_histogram("magpie_sync_duration_seconds")
            .with_description("Wall-clock duration of a single depot or mod sync")
            .with_unit("s")
            .build()
    })
}

/// Publishes the gauges that have to be sampled rather than incremented:
/// on-disk content size, and how many syncs are in flight against the
/// worker ceiling.
///
/// `syncing` is the same counter `GetSyncStatus` reports, so the metric
/// and the RPC can never disagree.
pub fn spawn(shared: Arc<Shared>) {
    let meter = observability::meter();
    let content_bytes = meter
        .u64_gauge("magpie_sync_content_bytes")
        .with_description("On-disk synced content by kind -- previously only visible by `du` inside the CSI node pod")
        .build();
    let in_flight = meter
        .u64_gauge("magpie_sync_in_flight")
        .with_description("Depot/mod syncs currently running")
        .build();
    // Published as a metric rather than left in config so the in-flight
    // count can be read against its ceiling on one graph, without an
    // operator having to know what DOWNLOAD_WORKERS was set to.
    let worker_ceiling = meter
        .u64_gauge("magpie_sync_download_workers")
        .with_description("Configured per-sync chunk-download concurrency ceiling")
        .build();

    tokio::spawn(async move {
        loop {
            content_bytes.record(
                shared.sync_state.total_mods_size().await,
                &[KeyValue::new("kind", "mods")],
            );
            content_bytes.record(
                shared.sync_state.total_game_files_size().await,
                &[KeyValue::new("kind", "game_files")],
            );
            in_flight.record(shared.in_flight() as u64, &[]);
            worker_ceiling.record(shared.download_workers as u64, &[]);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}
