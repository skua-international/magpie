use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use regex::Regex;
use steamdepot::connection::CmConnection;
use tokio::sync::Semaphore;

use crate::cache;
use crate::steam::{self, SyncTasks};

/// Patch `meta.cpp` (publishedid) and, if `replace_app_id`, `mod.cpp`
/// (appId) the same way the old Python `_process_mod` did.
fn patch_mod_metadata(mod_dir: &Path, mod_id: u64, replace_app_id: bool) -> Result<()> {
    let meta_cpp = mod_dir.join("meta.cpp");
    if meta_cpp.exists() {
        let data = std::fs::read_to_string(&meta_cpp)?;
        let re_publishedid = Regex::new(r"publishedid\s*=\s*0\s*;").unwrap();
        let re_protocole = Regex::new(r"protocole").unwrap();
        let mut new_data = re_publishedid
            .replace(&data, format!("publishedid = {mod_id};").as_str())
            .into_owned();
        new_data = re_protocole.replace_all(&new_data, "protocol").into_owned();
        if new_data != data {
            tracing::info!("[{mod_id}] Updating {}", meta_cpp.display());
            std::fs::write(&meta_cpp, new_data)?;
        }
    }

    let mod_cpp = mod_dir.join("mod.cpp");
    if mod_cpp.exists() && replace_app_id {
        if let Ok(data) = std::fs::read_to_string(&mod_cpp) {
            let re_appid = Regex::new(r"appId\s*=\s*\d+\s*;").unwrap();
            let new_data = re_appid.replace(&data, "appId = 0;").into_owned();
            if new_data != data {
                tracing::info!("[{mod_id}] Replacing appId in {}", mod_cpp.display());
                std::fs::write(&mod_cpp, new_data)?;
            }
        } else {
            tracing::warn!("[{mod_id}] got bad mod.cpp");
        }
    }

    Ok(())
}

/// Result of syncing a set of mod IDs: the `-mod=` params and the
/// directories that need `.bikey` files copied out once downloads finish.
pub struct SyncModsResult {
    pub mods: Vec<String>,
    pub key_dirs: Vec<PathBuf>,
}

/// Spawn downloads for every mod ID into the shared `tasks` pool -- does
/// not wait for them to finish. Every mod always goes through full
/// chunk-level verification (steam::resolve_workshop_items' manifest cache
/// + sync_depot's on-disk check), same guarantee as server depots. Only
/// the network round-trips (depot key, manifest bytes) get skipped on a
/// cache hit, not the actual correctness check. Each spawned download also
/// handles its own post-processing (meta.cpp/mod.cpp patching) once it
/// completes, since that has to happen after that specific item's content
/// is actually on disk.
///
/// Takes an already-resolved flat `mod_ids` list -- unlike the old
/// `preset()` this replaces, it does no fetching or parsing of its own
/// (no preset HTML, no collection expansion). That happens upstream now:
/// `workshop-parse` (controller-side) extracts candidate IDs from
/// whatever the caller was given, and `steam::resolve_source_ids`
/// (Steam-authenticated, so it correctly handles private/unlisted
/// content) expands any collections among them into their member mods.
pub async fn sync_mods(
    conn: &mut CmConnection,
    mod_ids: &[u64],
    cdlc_force: bool,
    server_root: &Path,
    sem: Arc<Semaphore>,
    tasks: &Mutex<SyncTasks>,
    sync_state: Arc<cache::SyncState>,
) -> Result<SyncModsResult> {
    let replace_app_id = !cdlc_force;
    let workshop_root = server_root.join("workshop");
    let mod_dirs: Vec<(u64, PathBuf)> = mod_ids.iter().map(|&id| (id, workshop_root.join(id.to_string()))).collect();

    // Every candidate goes through resolve_workshop_items, which handles
    // the depot-key/manifest caching itself (skipping network round-trips
    // on a hit) -- but always ends in a real sync_depot chunk verification,
    // same as server depots. Nothing here decides "trust it, skip
    // checking" based on cached metadata alone.
    let resolution = steam::resolve_workshop_items(conn, &workshop_root, mod_ids, &sync_state)
        .await
        .context("failed to resolve workshop items for download")?;

    let http = reqwest::Client::new();
    for item in resolution.items {
        let item_dir = workshop_root.join(item.published_file_id.to_string());
        let pool = resolution.cdn_pool.clone();
        let http = http.clone();
        let sync_state = sync_state.clone();
        let id = item.published_file_id;

        let fut = async move {
            steam::download_one_depot(item.plan, item_dir.clone(), http, pool, sync_state).await?;
            patch_mod_metadata(&item_dir, id, replace_app_id)?;
            tracing::info!("[{id}] Finished");
            Ok(())
        };
        steam::spawn_bounded(tasks, sem.clone(), fut);
    }

    Ok(SyncModsResult {
        mods: mod_dirs.iter().map(|(id, _)| format!("workshop/{id}")).collect(),
        key_dirs: mod_dirs.into_iter().map(|(_, dir)| dir).collect(),
    })
}
