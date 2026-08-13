//! Download-path instruments.
//!
//! Here rather than in sync-daemon because the counts only exist inside
//! this crate's own download loop -- sync-daemon can see that a sync ran
//! and how long it took, but not how many bytes crossed the wire while it
//! was running.
//!
//! Built lazily rather than at startup so this crate stays usable by a
//! caller that never installed a meter provider (the CLI, tests): the SDK
//! falls back to a no-op provider and the download path is unaffected.

use std::sync::OnceLock;

use opentelemetry::metrics::Counter;

/// Bytes pulled from Steam's CDN, labelled by depot.
///
/// A monotonic counter, not a rate gauge: throughput comes from
/// `rate(magpie_sync_downloaded_bytes_total[5m])`, which stays correct
/// across scrape gaps and restarts in a way a self-computed instantaneous
/// rate does not.
pub fn downloaded_bytes() -> &'static Counter<u64> {
    static C: OnceLock<Counter<u64>> = OnceLock::new();
    C.get_or_init(|| {
        observability::meter()
            .u64_counter("magpie_sync_downloaded_bytes_total")
            .with_description("Bytes downloaded from Steam's CDN; rate() this for throughput")
            .with_unit("By")
            .build()
    })
}

/// Chunks processed, by whether they were already valid on disk or had to
/// be fetched. See the call site for why the split is the useful part.
pub fn chunks() -> &'static Counter<u64> {
    static C: OnceLock<Counter<u64>> = OnceLock::new();
    C.get_or_init(|| {
        observability::meter()
            .u64_counter("magpie_sync_chunks_total")
            .with_description("Chunks processed, by outcome (verified on disk vs downloaded)")
            .build()
    })
}
