//! Periodic gauge publisher for `magpie_servers_total{phase}` -- same
//! poll-and-set reasoning as services/registry's own metrics.rs (a
//! "list everything and derive a total" number, not worth incremental
//! bookkeeping on every mutation).

use std::time::Duration;

use crd::{ArmaServer, ArmaServerPhase};
use kube::Client;
use kube::api::{Api, ListParams};
use opentelemetry::KeyValue;
use tracing::warn;

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const PHASES: [ArmaServerPhase; 4] = [
    ArmaServerPhase::Stopped,
    ArmaServerPhase::Pending,
    ArmaServerPhase::Running,
    ArmaServerPhase::Failed,
];

pub fn spawn(client: Client, namespace: String) {
    // Built once outside the loop: re-resolving an instrument against the
    // meter provider on every poll is wasted work, and the `metrics`
    // macros this replaces cached for the same reason.
    let servers_total = observability::meter()
        .u64_gauge("magpie_servers_total")
        .with_description("ArmaServers by reconciliation phase")
        .build();
    tokio::spawn(async move {
        let api: Api<ArmaServer> = Api::namespaced(client, &namespace);
        loop {
            match api.list(&ListParams::default()).await {
                Ok(servers) => {
                    let mut counts = [0u64; PHASES.len()];
                    for obj in &servers.items {
                        let phase = obj.status.clone().unwrap_or_default().phase;
                        if let Some(i) = PHASES.iter().position(|p| *p == phase) {
                            counts[i] += 1;
                        }
                    }
                    // Every phase gets set, including 0, so the metric
                    // always exists rather than only appearing once a
                    // server has actually reached it.
                    for (phase, count) in PHASES.iter().zip(counts) {
                        let label = format!("{phase:?}").to_lowercase();
                        servers_total.record(count, &[KeyValue::new("phase", label)]);
                    }
                }
                Err(e) => warn!("metrics poll failed: {e:#}"),
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}
