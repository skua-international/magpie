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
use sqlx::PgPool;
use sync_client::SyncClient;
use tracing::warn;

use crate::service::mod_source::to_mod_source_info;

const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Spawns the poll loop in the background -- fire-and-forget, same as
/// any other long-running task in these services; a single failed poll
/// (e.g. a transient Postgres/K8s API hiccup) just logs and retries next
/// interval rather than crashing the process.
pub fn spawn(client: Client, namespace: String, pool: PgPool, sync_client: Arc<SyncClient>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = poll_once(&client, &namespace, &pool, &sync_client).await {
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
            metrics::gauge!("magpie_local_mod_size_bytes", "unique_id" => info.reference)
                .set(info.size_bytes as f64);
        }
    }
    for (kind, count) in &by_kind {
        metrics::gauge!("magpie_mod_sources_total", "kind" => kind.clone()).set(*count as f64);
    }
    metrics::gauge!("magpie_local_mods_total").set(local_count as f64);
    metrics::gauge!("magpie_local_mods_size_bytes_total").set(local_bytes as f64);

    let synced = sync_client.list_synced_mods().await?;
    metrics::gauge!("magpie_synced_mods_total").set(synced.len() as f64);
    for m in &synced {
        metrics::gauge!("magpie_mod_size_bytes", "mod_id" => m.mod_id.to_string())
            .set(m.size_bytes as f64);
    }

    let missions = registry_db::list_missions(pool).await?;
    let missions_bytes: u64 = missions.iter().map(|m| m.filesize as u64).sum();
    metrics::gauge!("magpie_missions_total").set(missions.len() as f64);
    metrics::gauge!("magpie_missions_size_bytes_total").set(missions_bytes as f64);
    for m in &missions {
        metrics::gauge!("magpie_mission_size_bytes", "mission_id" => m.id.to_string())
            .set(m.filesize as f64);
    }

    let stats = sync_client.sync_stats().await?;
    metrics::gauge!("magpie_disk_usage_bytes", "kind" => "mods").set(stats.mods_bytes as f64);
    metrics::gauge!("magpie_disk_usage_bytes", "kind" => "game_files")
        .set(stats.game_files_bytes as f64);
    metrics::gauge!("magpie_disk_usage_bytes", "kind" => "missions").set(missions_bytes as f64);

    Ok(())
}
