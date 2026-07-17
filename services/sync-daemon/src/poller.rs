//! Periodically re-resolves every registered source's original candidate
//! IDs, independent of whether anything calls `RegisterSource` again --
//! catches a collection's membership drifting (mods added/removed
//! upstream) even for a source nothing is currently actively reconciling
//! (e.g. a mod pack kept "chambered" with no server referencing it right
//! now). `RegisterSource`-triggered refreshes (driven by a controller's
//! own reconcile cadence, once that exists) handle the common case faster;
//! this is the backstop.

use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info, warn};

use crate::service::Shared;

pub fn spawn(shared: Arc<Shared>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // first tick fires immediately; skip it, sources won't exist yet at startup
        loop {
            ticker.tick().await;
            run_once(&shared).await;
        }
    });
}

async fn run_once(shared: &Arc<Shared>) {
    let sources = match shared.sync_state.all_source_candidates() {
        Ok(s) => s,
        Err(e) => {
            error!("poller: failed to list sources: {e:#}");
            return;
        }
    };
    if sources.is_empty() {
        return;
    }
    info!("poller: re-resolving {} source(s)", sources.len());
    for (source_id, candidate_ids) in sources {
        if let Err(e) = shared.register_source_impl(&candidate_ids, &source_id).await {
            warn!("poller: failed to re-resolve source {source_id}: {e:#}");
        }
    }
}
