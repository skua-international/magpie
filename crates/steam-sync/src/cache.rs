use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use steamdepot::cdn::DepotManifest;

/// Cache lives alongside the actual downloaded content (`install_dir`,
/// e.g. `/arma3/server`), not a separate Docker volume -- that path is
/// already a host bind mount that survives container recreation (not just
/// restarts), and tying the cache to the same location it describes means
/// wiping the content for a clean reinstall wipes the cache with it, with
/// no separate invalidation logic needed.
fn cache_dir(install_dir: &Path) -> PathBuf {
    install_dir.join(".cache")
}

fn refresh_token_file(install_dir: &Path) -> PathBuf {
    cache_dir(install_dir).join("steam_refresh_token")
}

/// Steam's refresh token from the last successful credentialed login --
/// long-lived (on the order of months, not the ~24h access token it hands
/// out alongside it), so persisting it means most process restarts can skip
/// the expensive RSA-key/begin-auth-session/poll negotiation entirely and
/// go straight to the cheap single-`ClientLogon` path, same speed as
/// anonymous login. The caller is expected to *try* logging on with this
/// and fall back to renegotiating from scratch if Steam rejects it
/// (revoked, expired, password changed, ...) -- this is a speed
/// optimization, not something trusted on its own.
pub fn load_refresh_token(install_dir: &Path) -> Option<String> {
    let s = std::fs::read_to_string(refresh_token_file(install_dir)).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

pub fn save_refresh_token(install_dir: &Path, token: &str) -> Result<()> {
    let path = refresh_token_file(install_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("failed to create cache dir")?;
    }
    std::fs::write(&path, token).context("failed to write refresh token cache")?;
    // It's a live credential, not just cache metadata -- lock the file down
    // to the owning user regardless of the container's default umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .context("failed to set refresh token cache file permissions")?;
    }
    Ok(())
}

fn keys_file(install_dir: &Path) -> PathBuf {
    cache_dir(install_dir).join("depot_keys.json")
}

/// Depot decryption keys don't rotate for a given depot_id, so these are
/// safe to cache indefinitely once fetched.
pub fn load_keys(install_dir: &Path) -> HashMap<u32, Vec<u8>> {
    let Ok(data) = std::fs::read_to_string(keys_file(install_dir)) else {
        return HashMap::new();
    };
    let Ok(hex_map) = serde_json::from_str::<HashMap<u32, String>>(&data) else {
        return HashMap::new();
    };
    hex_map
        .into_iter()
        .filter_map(|(id, hex)| hex_to_bytes(&hex).map(|b| (id, b)))
        .collect()
}

pub fn save_keys(install_dir: &Path, keys: &HashMap<u32, Vec<u8>>) -> Result<()> {
    std::fs::create_dir_all(cache_dir(install_dir)).context("failed to create cache dir")?;
    let hex_map: HashMap<u32, String> = keys.iter().map(|(id, k)| (*id, bytes_to_hex(k))).collect();
    let data = serde_json::to_string_pretty(&hex_map).context("failed to serialize depot key cache")?;
    std::fs::write(keys_file(install_dir), data).context("failed to write depot key cache")?;
    Ok(())
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn manifest_path(install_dir: &Path, depot_id: u32, manifest_id: u64) -> PathBuf {
    cache_dir(install_dir)
        .join("manifests")
        .join(format!("{depot_id}_{manifest_id}.bin"))
}

/// Load a cached manifest for this exact depot_id + manifest_id. A
/// mismatch on either (new manifest_id from a content update, or a depot
/// we haven't seen before) is a cache miss -- the caller re-fetches from
/// the CDN as usual and calls [`save_manifest`] to cache the result.
pub fn load_manifest(install_dir: &Path, depot_id: u32, manifest_id: u64) -> Option<DepotManifest> {
    let data = std::fs::read(manifest_path(install_dir, depot_id, manifest_id)).ok()?;
    bincode::deserialize(&data).ok()
}

pub fn save_manifest(install_dir: &Path, depot_id: u32, manifest_id: u64, manifest: &DepotManifest) -> Result<()> {
    let path = manifest_path(install_dir, depot_id, manifest_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("failed to create manifest cache dir")?;
    }
    let data = bincode::serialize(manifest).context("failed to serialize manifest for cache")?;
    std::fs::write(path, data).context("failed to write manifest cache")?;
    Ok(())
}

/// Key identifying one synced thing within [`SyncState`]: depot_id alone
/// isn't unique (every Arma 3 workshop item shares depot_id == consumer app
/// 107410), so this pairs it with the install directory's own last path
/// component (the published file ID for a mod, the server root's name for
/// a server/CDLC depot -- same disambiguation the log `tag` in
/// `download_one_depot` already uses).
pub fn sync_key(depot_id: u32, install_dir: &Path) -> String {
    let leaf = install_dir.file_name().and_then(|n| n.to_str()).unwrap_or("?");
    format!("{depot_id}/{leaf}")
}

/// How long a lock row can go unrenewed before another process is allowed to
/// steal it -- covers a process that crashed (container OOM-killed, etc.)
/// mid-sync without ever reaching the `LockGuard` release. Well above any
/// realistic single depot/mod sync time.
const STALE_LOCK_SECS: i64 = 30 * 60;

const LOCK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

/// Identifies which process holds a lock, for logging only (not itself part
/// of the correctness story -- the DB's own locking is what actually
/// serializes concurrent acquirers).
fn holder_id() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into());
    format!("{host}:{}", std::process::id())
}

/// Cross-process state and locking for "has this depot/mod already been
/// fully chunk-verified at its current manifest_id" -- backed by SQLite
/// (not a hand-rolled JSON file) specifically because more than one server
/// instance can share the same workshop/server content directory. A plain
/// `std::fs::write` has no cross-process atomicity at all (not even a
/// temp-file+rename), and an in-process `Mutex` only serializes writers
/// within one process -- neither stops two separate launcher processes from
/// corrupting the same file, or worse, both writing into the same depot's
/// files on disk at once. SQLite's own file locking (`BEGIN IMMEDIATE`
/// transactions) gives real cross-process mutual exclusion for both the
/// state table and the advisory lock table below.
pub struct SyncState {
    conn: Mutex<Connection>,
}

impl SyncState {
    pub fn open(root: &Path) -> Result<Arc<Self>> {
        let dir = cache_dir(root);
        std::fs::create_dir_all(&dir).context("failed to create cache dir")?;
        let conn = Connection::open(dir.join("sync.db")).context("failed to open sync state database")?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("failed to set sync state database to WAL mode")?;
        conn.pragma_update(None, "busy_timeout", 5000u32)
            .context("failed to set sync state database busy_timeout")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS synced (
                key TEXT PRIMARY KEY,
                manifest_id INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS locks (
                key TEXT PRIMARY KEY,
                holder TEXT NOT NULL,
                acquired_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sources (
                source_id TEXT PRIMARY KEY,
                candidate_ids TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS source_mods (
                source_id TEXT NOT NULL,
                mod_id INTEGER NOT NULL,
                PRIMARY KEY (source_id, mod_id)
            );",
        )
        .context("failed to initialize sync state schema")?;
        Ok(Arc::new(Self { conn: Mutex::new(conn) }))
    }

    /// Record `source_id`'s originally-registered candidate IDs (the raw
    /// input to `resolve_source_ids`, not the resolved mod list) -- used by
    /// the background poller to know what to re-resolve later. Idempotent:
    /// re-registering the same `source_id` just overwrites its candidates.
    pub fn upsert_source(&self, source_id: &str, candidate_ids: &[u64]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let json = serde_json::to_string(candidate_ids).context("failed to serialize candidate_ids")?;
        conn.execute(
            "INSERT INTO sources (source_id, candidate_ids) VALUES (?1, ?2)
             ON CONFLICT(source_id) DO UPDATE SET candidate_ids = excluded.candidate_ids",
            params![source_id, json],
        )
        .context("failed to upsert source")?;
        Ok(())
    }

    /// Replace `source_id`'s current resolved membership with exactly
    /// `mod_ids` -- inserts newly-appeared entries, deletes ones no longer
    /// present. A mod dropping out of `source_id`'s membership here doesn't
    /// necessarily stop it from being desired overall; see
    /// [`desired_mod_ids`](Self::desired_mod_ids), which unions across
    /// every source.
    pub fn set_source_mods(&self, source_id: &str, mod_ids: &[u64]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().context("failed to begin source_mods update transaction")?;
        let existing: std::collections::HashSet<u64> = {
            let mut stmt = tx.prepare("SELECT mod_id FROM source_mods WHERE source_id = ?1")?;
            stmt.query_map([source_id], |r| r.get::<_, i64>(0))?
                .filter_map(|r| r.ok())
                .map(|v| v as u64)
                .collect()
        };
        let wanted: std::collections::HashSet<u64> = mod_ids.iter().copied().collect();

        for mod_id in wanted.difference(&existing) {
            tx.execute(
                "INSERT INTO source_mods (source_id, mod_id) VALUES (?1, ?2)",
                params![source_id, *mod_id as i64],
            )?;
        }
        for mod_id in existing.difference(&wanted) {
            tx.execute(
                "DELETE FROM source_mods WHERE source_id = ?1 AND mod_id = ?2",
                params![source_id, *mod_id as i64],
            )?;
        }
        tx.commit().context("failed to commit source_mods update")?;
        Ok(())
    }

    /// Explicit, deliberate removal of a source and its membership rows.
    /// Does not touch any *other* source's rows -- a mod this source also
    /// referenced but that's still claimed by another source keeps syncing.
    pub fn delete_source(&self, source_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sources WHERE source_id = ?1", [source_id])
            .context("failed to delete source")?;
        conn.execute("DELETE FROM source_mods WHERE source_id = ?1", [source_id])
            .context("failed to delete source_mods")?;
        Ok(())
    }

    /// The full set of mod IDs desired by *any* currently-registered
    /// source -- what an actual sync pass should treat as "wanted".
    pub fn desired_mod_ids(&self) -> Result<Vec<u64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT DISTINCT mod_id FROM source_mods")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .map(|v| v as u64)
            .collect();
        Ok(ids)
    }

    /// A single source's originally-registered candidate IDs, for an
    /// on-demand refresh (`RefreshSource`) rather than the background
    /// poller's full sweep. `None` if `source_id` was never registered (or
    /// was since deregistered).
    pub fn candidate_ids_for_source(&self, source_id: &str) -> Result<Option<Vec<u64>>> {
        let conn = self.conn.lock().unwrap();
        let json: Option<String> =
            conn.query_row("SELECT candidate_ids FROM sources WHERE source_id = ?1", [source_id], |r| r.get(0)).optional()?;
        Ok(json.map(|json| serde_json::from_str(&json).unwrap_or_default()))
    }

    /// A single source's current resolved mod list -- a plain read of
    /// whatever `RegisterSource`/`RefreshSource`/the background poller last
    /// resolved, no Steam calls. Used by `GetSourceMods`.
    pub fn mod_ids_for_source(&self, source_id: &str) -> Result<Vec<u64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT mod_id FROM source_mods WHERE source_id = ?1")?;
        let ids = stmt.query_map([source_id], |r| r.get::<_, i64>(0))?.filter_map(|r| r.ok()).map(|v| v as u64).collect();
        Ok(ids)
    }

    /// Every registered source's original candidate IDs, for the
    /// background poller to periodically re-resolve (picking up upstream
    /// collection membership changes even if nothing re-registers them).
    pub fn all_source_candidates(&self) -> Result<Vec<(String, Vec<u64>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT source_id, candidate_ids FROM sources")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (source_id, json) = row?;
            let candidate_ids: Vec<u64> = serde_json::from_str(&json).unwrap_or_default();
            out.push((source_id, candidate_ids));
        }
        Ok(out)
    }

    /// Whether `key` was last verified at exactly `manifest_id`, and the
    /// content is still on disk (a marker alone doesn't prove that -- the
    /// directory could've been wiped since). A match against the
    /// freshly-resolved current manifest_id (never itself cached -- always
    /// this run's live PICS/GetDetails result) is a sound proof that
    /// nothing changed since the last verified sync, since Steam's
    /// manifest_id changes whenever a depot's content changes.
    pub fn is_synced(&self, key: &str, manifest_id: u64, dir_exists: bool) -> bool {
        if !dir_exists {
            return false;
        }
        self.last_manifest_id(key) == Some(manifest_id)
    }

    /// The manifest_id `key` was last verified at, if any.
    pub fn last_manifest_id(&self, key: &str) -> Option<u64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT manifest_id FROM synced WHERE key = ?1", [key], |r| r.get::<_, i64>(0))
            .optional()
            .ok()
            .flatten()
            .map(|v| v as u64)
    }

    /// The entire `synced` table as an in-memory snapshot, for resolution
    /// loops that need to check many keys at once -- one query instead of
    /// one per item avoids paying SQLite's per-call lock-acquire/query/
    /// release round-trip (real, measurable time across 100+ workshop
    /// items) for what's otherwise a pure in-memory membership check.
    ///
    /// Safe to use for the resolve-time skip decision specifically because
    /// it's only ever a fast-path hint -- `download_one_depot` still does a
    /// live, lock-protected [`is_synced`](Self::is_synced) re-check before
    /// doing any real work, so a microseconds-stale snapshot can cause at
    /// worst a missed skip (an item gets dispatched, then the live
    /// re-check catches that it's already synced), never an incorrect one.
    pub fn snapshot(&self) -> HashMap<String, u64> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT key, manifest_id FROM synced") {
            Ok(stmt) => stmt,
            Err(e) => {
                tracing::warn!("failed to prepare sync state snapshot query: {e:#}");
                return HashMap::new();
            }
        };
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)));
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                tracing::warn!("failed to read sync state snapshot: {e:#}");
                HashMap::new()
            }
        }
    }

    /// Record that `key` was just fully chunk-verified at `manifest_id`.
    /// Call this only after [`steamdepot::download::sync_depot`] reports
    /// success, never speculatively, since this is later trusted to skip
    /// verification outright. Logs and swallows a write failure rather than
    /// propagating it -- losing this run's persistence is a slower next
    /// run, not a correctness problem worth failing an otherwise-successful
    /// sync over.
    pub fn mark_synced(&self, key: &str, manifest_id: u64) {
        let conn = self.conn.lock().unwrap();
        let result = conn.execute(
            "INSERT INTO synced (key, manifest_id) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET manifest_id = excluded.manifest_id",
            params![key, manifest_id as i64],
        );
        if let Err(e) = result {
            tracing::warn!("failed to persist sync state for {key}: {e:#}");
        }
    }

    /// Try once to acquire the cross-process lock for `key`. `Ok(true)`
    /// means the caller now holds it (until the returned drop happens via
    /// [`acquire_lock`](Self::acquire_lock)'s `LockGuard`); `Ok(false)`
    /// means someone else currently holds it.
    fn try_acquire(&self, key: &str, holder: &str) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let now = now_unix();

        // Fast path: a plain insert. The PRIMARY KEY constraint is the
        // actual atomicity guarantee here -- if two processes race this at
        // once, SQLite's own locking serializes their writes, and exactly
        // one INSERT succeeds.
        match conn.execute(
            "INSERT INTO locks (key, holder, acquired_at) VALUES (?1, ?2, ?3)",
            params![key, holder, now],
        ) {
            Ok(_) => return Ok(true),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {}
            Err(e) => return Err(e).context("failed to insert lock row"),
        }

        // Someone already holds it, or held it and crashed without
        // releasing. Steal it only if clearly stale, inside one IMMEDIATE
        // transaction so a concurrent stale-check by another process can't
        // race us into double-stealing the same lock.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to begin lock-steal transaction")?;
        let existing: Option<i64> = tx
            .query_row("SELECT acquired_at FROM locks WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .context("failed to read existing lock")?;
        let stolen = match existing {
            Some(acquired_at) if now - acquired_at > STALE_LOCK_SECS => {
                tx.execute("DELETE FROM locks WHERE key = ?1", [key])
                    .context("failed to delete stale lock")?;
                tx.execute(
                    "INSERT INTO locks (key, holder, acquired_at) VALUES (?1, ?2, ?3)",
                    params![key, holder, now],
                )
                .context("failed to insert lock row after stealing")?;
                true
            }
            // Still held and fresh, or was released between our failed
            // insert above and this check -- either way, not ours yet.
            // The latter case just costs one extra poll cycle.
            _ => false,
        };
        tx.commit().context("failed to commit lock-steal transaction")?;
        Ok(stolen)
    }

    fn release(&self, key: &str) {
        let conn = self.conn.lock().unwrap();
        if let Err(e) = conn.execute("DELETE FROM locks WHERE key = ?1", [key]) {
            tracing::warn!("failed to release lock for {key}: {e:#}");
        }
    }

    /// Acquire the cross-process lock for `key`, blocking (without tying up
    /// a tokio worker thread -- the actual DB calls run via
    /// `spawn_blocking`, and the wait between poll attempts is a real async
    /// sleep) until it's free. Multiple server instances can share one
    /// content directory; this keeps two of them from verifying/downloading
    /// the same depot's files at the same time.
    pub async fn acquire_lock(self: &Arc<Self>, key: String) -> Result<LockGuard> {
        let holder = holder_id();
        let mut waited = false;
        loop {
            let this = self.clone();
            let k = key.clone();
            let h = holder.clone();
            let acquired = tokio::task::spawn_blocking(move || this.try_acquire(&k, &h))
                .await
                .context("lock-acquire task panicked")??;
            if acquired {
                if waited {
                    tracing::info!("[{key}] acquired lock (was waiting on another process)");
                }
                return Ok(LockGuard { state: self.clone(), key });
            }
            if !waited {
                tracing::info!("[{key}] waiting for lock held by another server instance...");
                waited = true;
            }
            tokio::time::sleep(LOCK_POLL_INTERVAL).await;
        }
    }
}

/// Releases its lock on drop. Held across the double-checked verify pass in
/// `download_one_depot`: after acquiring it, re-check `is_synced` (another
/// process may have just finished this exact key while we waited) before
/// doing any real work.
pub struct LockGuard {
    state: Arc<SyncState>,
    key: String,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.state.release(&self.key);
    }
}
