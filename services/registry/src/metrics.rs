//! Periodic gauge publisher for registry's own business metrics, scraped
//! via `/metrics` (see main.rs) -- separate from the request-scoped RPC
//! counters `crates/authn`'s `require_auth` middleware records, since
//! these are all "list everything and derive a total" numbers already
//! computed the same way their RPC counterparts (`GetDiskUsage`,
//! `ListModSources`, `ListSyncedMods`, `ListMissions`) are, just polled
//! on an interval and published as gauges instead of returned per-call.
//! A poll-and-set on a timer can't drift out of sync the way manual
//! increment/decrement bookkeeping on every mutation could.

use std::sync::Arc;
use std::time::Duration;

use crd::ModSource;
use kube::Client;
use kube::api::{Api, ListParams};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Gauge;
use sqlx::PgPool;
use sync_client::SyncClient;
use tracing::warn;

use crate::service::mod_source::to_mod_source_info;

/// Every instrument this module publishes, built once rather than
/// re-resolved against the meter provider on each 30s poll.
struct Instruments {
    disk_usage_bytes: Gauge<u64>,
    mod_sources_total: Gauge<u64>,
    synced_mods_total: Gauge<u64>,
    mod_size_bytes: Gauge<u64>,
    missions_total: Gauge<u64>,
    missions_size_bytes_total: Gauge<u64>,
    mission_size_bytes: Gauge<u64>,
    local_mods_total: Gauge<u64>,
    local_mods_size_bytes_total: Gauge<u64>,
    local_mod_size_bytes: Gauge<u64>,
}

impl Instruments {
    fn new() -> Self {
        let meter = observability::meter();
        Self {
            disk_usage_bytes: meter
                .u64_gauge("magpie_disk_usage_bytes")
                .with_description("On-disk bytes by content kind, deduplicated across sources")
                .build(),
            mod_sources_total: meter
                .u64_gauge("magpie_mod_sources_total")
                .with_description("Registered mod sources by kind")
                .build(),
            synced_mods_total: meter
                .u64_gauge("magpie_synced_mods_total")
                .with_description("Workshop mods currently tracked as verified-synced")
                .build(),
            mod_size_bytes: meter
                .u64_gauge("magpie_mod_size_bytes")
                .with_description("On-disk size of one synced mod")
                .build(),
            missions_total: meter.u64_gauge("magpie_missions_total").build(),
            missions_size_bytes_total: meter.u64_gauge("magpie_missions_size_bytes_total").build(),
            mission_size_bytes: meter.u64_gauge("magpie_mission_size_bytes").build(),
            local_mods_total: meter.u64_gauge("magpie_local_mods_total").build(),
            local_mods_size_bytes_total: meter
                .u64_gauge("magpie_local_mods_size_bytes_total")
                .build(),
            local_mod_size_bytes: meter.u64_gauge("magpie_local_mod_size_bytes").build(),
        }
    }
}

const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Spawns the poll loop in the background -- fire-and-forget, same as
/// any other long-running task in these services; a single failed poll
/// (e.g. a transient Postgres/K8s API hiccup) just logs and retries next
/// interval rather than crashing the process.
pub fn spawn(client: Client, namespace: String, pool: PgPool, sync_client: Arc<SyncClient>) {
    let instruments = Instruments::new();
    tokio::spawn(async move {
        loop {
            if let Err(e) = poll_once(&client, &namespace, &pool, &sync_client, &instruments).await
            {
                warn!("metrics poll failed: {e:#}");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

async fn poll_once(
    client: &Client,
    namespace: &str,
    pool: &PgPool,
    sync_client: &SyncClient,
    instruments: &Instruments,
) -> anyhow::Result<()> {
    let api: Api<ModSource> = Api::namespaced(client.clone(), namespace);
    let mod_sources = api.list(&ListParams::default()).await?;

    let mut local_count = 0u64;
    let mut local_bytes = 0u64;
    let mut by_kind = std::collections::BTreeMap::<String, u64>::new();
    for obj in &mod_sources.items {
        let info = to_mod_source_info(obj);
        // Debug on these generated enums prints the exact proto variant
        // name (e.g. "MOD_SOURCE_KIND_LOCAL") -- no as_str_name() method
        // exists on buffa-generated enums the way protoc-gen-go's does.
        let kind = format!("{:?}", info.kind.as_known().unwrap_or_default());
        let is_local = kind == "MOD_SOURCE_KIND_LOCAL";
        *by_kind.entry(kind).or_default() += 1;
        if is_local {
            local_count += 1;
            local_bytes += info.size_bytes;
            instruments.local_mod_size_bytes.record(
                info.size_bytes,
                &[KeyValue::new("unique_id", info.reference)],
            );
        }
    }
    for (kind, count) in &by_kind {
        instruments
            .mod_sources_total
            .record(*count as u64, &[KeyValue::new("kind", kind.clone())]);
    }
    instruments.local_mods_total.record(local_count as u64, &[]);
    instruments
        .local_mods_size_bytes_total
        .record(local_bytes, &[]);

    let synced = sync_client.list_synced_mods().await?;
    instruments
        .synced_mods_total
        .record(synced.len() as u64, &[]);
    for m in &synced {
        instruments.mod_size_bytes.record(
            m.size_bytes,
            &[KeyValue::new("mod_id", m.mod_id.to_string())],
        );
    }

    let missions = registry_db::list_missions(pool).await?;
    let missions_bytes: u64 = missions.iter().map(|m| m.filesize as u64).sum();
    instruments
        .missions_total
        .record(missions.len() as u64, &[]);
    instruments
        .missions_size_bytes_total
        .record(missions_bytes as u64, &[]);
    for m in &missions {
        instruments.mission_size_bytes.record(
            m.filesize as u64,
            &[KeyValue::new("mission_id", m.id.to_string())],
        );
    }

    let stats = sync_client.sync_stats().await?;
    instruments
        .disk_usage_bytes
        .record(stats.mods_bytes, &[KeyValue::new("kind", "mods")]);
    instruments.disk_usage_bytes.record(
        stats.game_files_bytes,
        &[KeyValue::new("kind", "game_files")],
    );
    instruments
        .disk_usage_bytes
        .record(missions_bytes as u64, &[KeyValue::new("kind", "missions")]);

    Ok(())
}
