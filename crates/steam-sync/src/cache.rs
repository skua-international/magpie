use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use steamdepot::cdn::DepotManifest;
use tokio::sync::Mutex;
use turso::transaction::TransactionBehavior;

/// One workshop mod's synced state, as returned by
/// [`SyncState::list_synced_mods`]/[`SyncState::get_synced_mod`].
pub struct SyncedModRow {
    pub mod_id: u64,
    pub manifest_id: u64,
    pub size_bytes: u64,
    /// Empty if this mod was never seen via `record_mod_title` in this
    /// cache's lifetime -- see that method's own doc.
    pub title: String,
}

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
    let data =
        serde_json::to_string_pretty(&hex_map).context("failed to serialize depot key cache")?;
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

pub fn save_manifest(
    install_dir: &Path,
    depot_id: u32,
    manifest_id: u64,
    manifest: &DepotManifest,
) -> Result<()> {
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
    let leaf = install_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?");
    format!("{depot_id}/{leaf}")
}

/// How long a lock row can go unrenewed before another process is allowed to
/// steal it -- covers a process that crashed (container OOM-killed, etc.)
/// mid-sync without ever reaching the `LockGuard` release. Well above any
/// realistic single depot/mod sync time.
const STALE_LOCK_SECS: i64 = 30 * 60;

const LOCK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Identifies which process holds a lock, for logging only (not itself part
/// of the correctness story -- the DB's own locking is what actually
/// serializes concurrent acquirers).
fn holder_id() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into());
    format!("{host}:{}", std::process::id())
}

/// Returns `Ok(None)` for `Error::QueryReturnedNoRows` instead of
/// propagating it -- turso's analog of rusqlite's `.optional()` extension,
/// which turso doesn't provide itself.
async fn optional_row(
    stmt: &mut turso::Statement,
    params: impl turso::IntoParams,
) -> Result<Option<turso::Row>> {
    match stmt.query_row(params).await {
        Ok(row) => Ok(Some(row)),
        Err(turso::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Cross-process state and locking for "has this depot/mod already been
/// fully chunk-verified at its current manifest_id" -- backed by SQLite
/// (not a hand-rolled JSON file) specifically because more than one server
/// instance can share the same workshop/server content directory. A plain
/// `std::fs::write` has no cross-process atomicity at all (not even a
/// temp-file+rename), and an in-process lock only serializes writers
/// within one process -- neither stops two separate launcher processes from
/// corrupting the same file, or worse, both writing into the same depot's
/// files on disk at once. SQLite's own file locking (`BEGIN IMMEDIATE`
/// transactions) gives real cross-process mutual exclusion for both the
/// state table and the advisory lock table below.
///
/// `conn` is wrapped in a `tokio::sync::Mutex` (async-aware, doesn't block a
/// worker thread while waiting) despite turso's `Connection` being `Clone` +
/// `Send` + `Sync` -- that only means the *handle* is safely shareable, not
/// that concurrent operations on the underlying session are safe. Confirmed
/// live: turso's own `ConcurrentGuard` (sdk-kit/src/lib.rs) rejects any
/// overlapping operation on the same session with `Misuse("concurrent use
/// forbidden")`, regardless of which cloned handle initiates it -- two
/// ModSources' auto-sync tasks racing `acquire_lock` concurrently hit this
/// within minutes of the rusqlite->turso migration shipping without this
/// Mutex. rusqlite's own `Connection` needed the same serialization for the
/// same underlying reason (a single connection/session is not itself
/// safe for concurrent use), just via `std::sync::Mutex` there since
/// rusqlite is fully synchronous.
pub struct SyncState {
    conn: Mutex<turso::Connection>,
}

impl SyncState {
    pub async fn open(root: &Path) -> Result<Arc<Self>> {
        let dir = cache_dir(root);
        std::fs::create_dir_all(&dir).context("failed to create cache dir")?;
        let db_path = dir
            .join("sync.db")
            .to_str()
            .context("cache dir path is not valid UTF-8")?
            .to_string();
        // experimental_multiprocess_wal: this cache is shared across
        // multiple server instances on the same content directory (see
        // this struct's own doc) -- needs real testing under actual
        // concurrent-process access before fully trusting it, same as any
        // experimental flag.
        let db = turso::Builder::new_local(&db_path)
            .experimental_multiprocess_wal(true)
            .build()
            .await
            .context("failed to open sync state database")?;
        let conn = db
            .connect()
            .context("failed to open sync state connection")?;
        conn.pragma_update("journal_mode", "WAL")
            .await
            .context("failed to set sync state database to WAL mode")?;
        conn.pragma_update("busy_timeout", 5000u32)
            .await
            .context("failed to set sync state database busy_timeout")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS synced (
                key TEXT PRIMARY KEY,
                manifest_id INTEGER NOT NULL,
                size_bytes INTEGER NOT NULL DEFAULT 0
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
            );
            CREATE TABLE IF NOT EXISTS mod_titles (
                mod_id INTEGER PRIMARY KEY,
                title TEXT NOT NULL
            );",
        )
        .await
        .context("failed to initialize sync state schema")?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    /// Record `source_id`'s originally-registered candidate IDs (the raw
    /// input to `resolve_source_ids`, not the resolved mod list) -- used by
    /// the background poller to know what to re-resolve later. Idempotent:
    /// re-registering the same `source_id` just overwrites its candidates.
    pub async fn upsert_source(&self, source_id: &str, candidate_ids: &[u64]) -> Result<()> {
        let json =
            serde_json::to_string(candidate_ids).context("failed to serialize candidate_ids")?;
        self.conn
            .lock()
            .await
            .execute(
                "INSERT INTO sources (source_id, candidate_ids) VALUES (?1, ?2)
                 ON CONFLICT(source_id) DO UPDATE SET candidate_ids = excluded.candidate_ids",
                turso::params![source_id, json],
            )
            .await
            .context("failed to upsert source")?;
        Ok(())
    }

    /// Replace `source_id`'s current resolved membership with exactly
    /// `mod_ids` -- inserts newly-appeared entries, deletes ones no longer
    /// present. A mod dropping out of `source_id`'s membership here doesn't
    /// necessarily stop it from being desired overall; see
    /// [`desired_mod_ids`](Self::desired_mod_ids), which unions across
    /// every source.
    pub async fn set_source_mods(&self, source_id: &str, mod_ids: &[u64]) -> Result<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn
            .transaction()
            .await
            .context("failed to begin source_mods update transaction")?;
        let existing: std::collections::HashSet<u64> = {
            let mut rows = tx
                .query(
                    "SELECT mod_id FROM source_mods WHERE source_id = ?1",
                    turso::params![source_id],
                )
                .await
                .context("failed to query existing source_mods")?;
            let mut set = std::collections::HashSet::new();
            while let Some(row) = rows.next().await.context("failed to read source_mods row")? {
                if let Ok(v) = row.get::<i64>(0) {
                    set.insert(v as u64);
                }
            }
            set
        };
        let wanted: std::collections::HashSet<u64> = mod_ids.iter().copied().collect();

        for mod_id in wanted.difference(&existing) {
            tx.execute(
                "INSERT INTO source_mods (source_id, mod_id) VALUES (?1, ?2)",
                turso::params![source_id, *mod_id as i64],
            )
            .await
            .context("failed to insert source_mods row")?;
        }
        for mod_id in existing.difference(&wanted) {
            tx.execute(
                "DELETE FROM source_mods WHERE source_id = ?1 AND mod_id = ?2",
                turso::params![source_id, *mod_id as i64],
            )
            .await
            .context("failed to delete source_mods row")?;
        }
        tx.commit()
            .await
            .context("failed to commit source_mods update")?;
        Ok(())
    }

    /// Explicit, deliberate removal of a source and its membership rows.
    /// Does not touch any *other* source's rows -- a mod this source also
    /// referenced but that's still claimed by another source keeps syncing.
    pub async fn delete_source(&self, source_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM sources WHERE source_id = ?1",
            turso::params![source_id],
        )
        .await
        .context("failed to delete source")?;
        conn.execute(
            "DELETE FROM source_mods WHERE source_id = ?1",
            turso::params![source_id],
        )
        .await
        .context("failed to delete source_mods")?;
        Ok(())
    }

    /// The full set of mod IDs desired by *any* currently-registered
    /// source -- what an actual sync pass should treat as "wanted".
    pub async fn desired_mod_ids(&self) -> Result<Vec<u64>> {
        let mut rows = self
            .conn
            .lock()
            .await
            .query("SELECT DISTINCT mod_id FROM source_mods", ())
            .await
            .context("failed to query desired_mod_ids")?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await.context("failed to read desired_mod_ids row")? {
            if let Ok(v) = row.get::<i64>(0) {
                ids.push(v as u64);
            }
        }
        Ok(ids)
    }

    /// A single source's originally-registered candidate IDs, for an
    /// on-demand refresh (`RefreshSource`) rather than the background
    /// poller's full sweep. `None` if `source_id` was never registered (or
    /// was since deregistered).
    pub async fn candidate_ids_for_source(&self, source_id: &str) -> Result<Option<Vec<u64>>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT candidate_ids FROM sources WHERE source_id = ?1")
            .await
            .context("failed to prepare candidate_ids_for_source query")?;
        let json: Option<String> = optional_row(&mut stmt, turso::params![source_id])
            .await?
            .map(|row| row.get(0))
            .transpose()
            .context("failed to read candidate_ids")?;
        Ok(json.map(|json| serde_json::from_str(&json).unwrap_or_default()))
    }

    /// A single source's current resolved mod list -- a plain read of
    /// whatever `RegisterSource`/`RefreshSource`/the background poller last
    /// resolved, no Steam calls. Used by `GetSourceMods`.
    pub async fn mod_ids_for_source(&self, source_id: &str) -> Result<Vec<u64>> {
        let mut rows = self
            .conn
            .lock()
            .await
            .query(
                "SELECT mod_id FROM source_mods WHERE source_id = ?1",
                turso::params![source_id],
            )
            .await
            .context("failed to query mod_ids_for_source")?;
        let mut ids = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .context("failed to read mod_ids_for_source row")?
        {
            if let Ok(v) = row.get::<i64>(0) {
                ids.push(v as u64);
            }
        }
        Ok(ids)
    }

    /// Every registered source's original candidate IDs, for the
    /// background poller to periodically re-resolve (picking up upstream
    /// collection membership changes even if nothing re-registers them).
    pub async fn all_source_candidates(&self) -> Result<Vec<(String, Vec<u64>)>> {
        let mut rows = self
            .conn
            .lock()
            .await
            .query("SELECT source_id, candidate_ids FROM sources", ())
            .await
            .context("failed to query all_source_candidates")?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .context("failed to read all_source_candidates row")?
        {
            let source_id: String = row.get(0).context("failed to read source_id")?;
            let json: String = row.get(1).context("failed to read candidate_ids")?;
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
    pub async fn is_synced(&self, key: &str, manifest_id: u64, dir_exists: bool) -> bool {
        if !dir_exists {
            return false;
        }
        self.last_manifest_id(key).await == Some(manifest_id)
    }

    /// The manifest_id `key` was last verified at, if any.
    pub async fn last_manifest_id(&self, key: &str) -> Option<u64> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT manifest_id FROM synced WHERE key = ?1")
            .await
            .ok()?;
        optional_row(&mut stmt, turso::params![key])
            .await
            .ok()
            .flatten()?
            .get::<i64>(0)
            .ok()
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
    pub async fn snapshot(&self) -> HashMap<String, u64> {
        let conn = self.conn.lock().await;
        let mut rows = match conn.query("SELECT key, manifest_id FROM synced", ()).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("failed to prepare sync state snapshot query: {e:#}");
                return HashMap::new();
            }
        };
        let mut out = HashMap::new();
        loop {
            match rows.next().await {
                Ok(Some(row)) => {
                    if let (Ok(k), Ok(v)) = (row.get::<String>(0), row.get::<i64>(1)) {
                        out.insert(k, v as u64);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("failed to read sync state snapshot: {e:#}");
                    break;
                }
            }
        }
        out
    }

    /// Clear `key`'s "last verified" marker only -- never touches files on
    /// disk. The next resolve pass then genuinely re-verifies this key's
    /// chunks against Steam's current manifest (via
    /// `steamdepot::download::sync_depot`, which always verifies before
    /// downloading anything -- see `download_one_depot`'s own comment),
    /// redownloading only whatever's actually missing or divergent, not a
    /// blind full wipe-and-refetch. This is the deliberately
    /// non-destructive half of "force refresh a mod" -- the caller-facing
    /// API surface (registry's `InvalidateMod`) is intentionally
    /// restricted to exactly this, not real file deletion.
    pub async fn invalidate(&self, key: &str) {
        if let Err(e) = self
            .conn
            .lock()
            .await
            .execute("DELETE FROM synced WHERE key = ?1", turso::params![key])
            .await
        {
            tracing::warn!("failed to invalidate sync state for {key}: {e:#}");
        }
    }

    /// Every currently-tracked workshop mod ID with its manifest_id,
    /// on-disk size, and title (empty if never seen by
    /// `record_mod_title`). Filtered to depot_id 107410 specifically --
    /// every Arma 3 workshop item shares that as its `consumer_appid`
    /// (see `sync_key`'s doc), which is how this distinguishes mod
    /// entries from server/CDLC depot entries (under app 233780) in the
    /// same table.
    pub async fn list_synced_mods(&self) -> Vec<SyncedModRow> {
        const WORKSHOP_CONSUMER_APP_ID: &str = "107410";
        let conn = self.conn.lock().await;
        let mut rows = match conn
            .query(
                "SELECT s.key, s.manifest_id, s.size_bytes, COALESCE(t.title, '')
                 FROM synced s LEFT JOIN mod_titles t ON t.mod_id = CAST(substr(s.key, instr(s.key, '/') + 1) AS INTEGER)",
                (),
            )
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("failed to prepare list_synced_mods query: {e:#}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        loop {
            let row = match rows.next().await {
                Ok(Some(row)) => row,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("failed to read list_synced_mods rows: {e:#}");
                    break;
                }
            };
            let (Ok(key), Ok(manifest_id), Ok(size_bytes), Ok(title)) = (
                row.get::<String>(0),
                row.get::<i64>(1),
                row.get::<i64>(2),
                row.get::<String>(3),
            ) else {
                continue;
            };
            let Some((depot, leaf)) = key.split_once('/') else {
                continue;
            };
            if depot != WORKSHOP_CONSUMER_APP_ID {
                continue;
            }
            if let Ok(mod_id) = leaf.parse::<u64>() {
                out.push(SyncedModRow {
                    mod_id,
                    manifest_id: manifest_id as u64,
                    size_bytes: size_bytes as u64,
                    title,
                });
            }
        }
        out
    }

    /// A single workshop mod's synced state, or `None` if it isn't
    /// currently tracked as synced at all.
    pub async fn get_synced_mod(&self, mod_id: u64) -> Option<SyncedModRow> {
        self.list_synced_mods()
            .await
            .into_iter()
            .find(|m| m.mod_id == mod_id)
    }

    /// Every source_id currently referencing `mod_id` -- a mod can be
    /// shared by more than one source (see `source_mods`'s own doc).
    pub async fn sources_for_mod(&self, mod_id: u64) -> Vec<String> {
        let conn = self.conn.lock().await;
        let mut rows = match conn
            .query(
                "SELECT source_id FROM source_mods WHERE mod_id = ?1",
                turso::params![mod_id as i64],
            )
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("failed to prepare sources_for_mod query: {e:#}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        loop {
            match rows.next().await {
                Ok(Some(row)) => {
                    if let Ok(source_id) = row.get::<String>(0) {
                        out.push(source_id);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("failed to read sources_for_mod rows: {e:#}");
                    break;
                }
            }
        }
        out
    }

    /// Total on-disk bytes across every currently-synced workshop mod,
    /// each counted exactly once regardless of how many sources reference
    /// it -- `synced` is keyed by mod, not by source, so this is naturally
    /// deduplicated already.
    pub async fn total_mods_size(&self) -> u64 {
        self.list_synced_mods()
            .await
            .iter()
            .map(|m| m.size_bytes)
            .sum()
    }

    /// Total on-disk bytes of every currently-synced server/CDLC depot
    /// entry (the base game + selected CDLCs) -- everything in `synced`
    /// that *isn't* a workshop mod (see `list_synced_mods`'s filter).
    pub async fn total_game_files_size(&self) -> u64 {
        const WORKSHOP_CONSUMER_APP_ID: &str = "107410";
        self.snapshot_with_size()
            .await
            .into_iter()
            .filter(|(key, _)| !key.starts_with(&format!("{WORKSHOP_CONSUMER_APP_ID}/")))
            .map(|(_, size_bytes)| size_bytes)
            .sum()
    }

    async fn snapshot_with_size(&self) -> Vec<(String, u64)> {
        let conn = self.conn.lock().await;
        let mut rows = match conn.query("SELECT key, size_bytes FROM synced", ()).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("failed to prepare snapshot_with_size query: {e:#}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        loop {
            match rows.next().await {
                Ok(Some(row)) => {
                    if let (Ok(k), Ok(v)) = (row.get::<String>(0), row.get::<i64>(1)) {
                        out.push((k, v as u64));
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("failed to read snapshot_with_size rows: {e:#}");
                    break;
                }
            }
        }
        out
    }

    /// Record/refresh a workshop mod's display title, seen from a real
    /// `resolve_source_ids`/`resolve_workshop_items` result -- the only
    /// place titles are ever known. Best-effort: a mod that's synced but
    /// was never resolved through either of those in this process' cache
    /// lifetime just has an empty title in `list_synced_mods`/
    /// `get_synced_mod` until the next time it is.
    pub async fn record_mod_title(&self, mod_id: u64, title: &str) {
        if title.is_empty() {
            return;
        }
        let result = self
            .conn
            .lock()
            .await
            .execute(
                "INSERT INTO mod_titles (mod_id, title) VALUES (?1, ?2)
                 ON CONFLICT(mod_id) DO UPDATE SET title = excluded.title",
                turso::params![mod_id as i64, title],
            )
            .await;
        if let Err(e) = result {
            tracing::warn!("failed to record title for mod {mod_id}: {e:#}");
        }
    }

    /// Record that `key` was just fully chunk-verified at `manifest_id`,
    /// with its current on-disk size. Call this only after
    /// [`steamdepot::download::sync_depot`] reports success, never
    /// speculatively, since this is later trusted to skip verification
    /// outright. Logs and swallows a write failure rather than propagating
    /// it -- losing this run's persistence is a slower next run, not a
    /// correctness problem worth failing an otherwise-successful sync
    /// over.
    pub async fn mark_synced(&self, key: &str, manifest_id: u64, size_bytes: u64) {
        let result = self
            .conn
            .lock()
            .await
            .execute(
                "INSERT INTO synced (key, manifest_id, size_bytes) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET manifest_id = excluded.manifest_id, size_bytes = excluded.size_bytes",
                turso::params![key, manifest_id as i64, size_bytes as i64],
            )
            .await;
        if let Err(e) = result {
            tracing::warn!("failed to persist sync state for {key}: {e:#}");
        }
    }

    /// Try once to acquire the cross-process lock for `key`. `Ok(true)`
    /// means the caller now holds it (until the returned drop happens via
    /// [`acquire_lock`](Self::acquire_lock)'s `LockGuard`); `Ok(false)`
    /// means someone else currently holds it.
    async fn try_acquire(&self, key: &str, holder: &str) -> Result<bool> {
        let now = now_unix();
        let mut conn = self.conn.lock().await;

        // Fast path: a plain insert. The PRIMARY KEY constraint is the
        // actual atomicity guarantee here -- if two processes race this at
        // once, SQLite's own locking serializes their writes, and exactly
        // one INSERT succeeds.
        match conn
            .execute(
                "INSERT INTO locks (key, holder, acquired_at) VALUES (?1, ?2, ?3)",
                turso::params![key, holder, now],
            )
            .await
        {
            Ok(_) => return Ok(true),
            Err(turso::Error::Constraint(_)) => {}
            Err(e) => return Err(e).context("failed to insert lock row"),
        }

        // Someone already holds it, or held it and crashed without
        // releasing. Steal it only if clearly stale, inside one IMMEDIATE
        // transaction so a concurrent stale-check by another process can't
        // race us into double-stealing the same lock.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .context("failed to begin lock-steal transaction")?;
        let mut stmt = tx
            .prepare("SELECT acquired_at FROM locks WHERE key = ?1")
            .await
            .context("failed to prepare existing-lock query")?;
        let existing: Option<i64> = optional_row(&mut stmt, turso::params![key])
            .await
            .context("failed to read existing lock")?
            .map(|row| row.get(0))
            .transpose()
            .context("failed to read acquired_at")?;
        drop(stmt);
        let stolen = match existing {
            Some(acquired_at) if now - acquired_at > STALE_LOCK_SECS => {
                tx.execute("DELETE FROM locks WHERE key = ?1", turso::params![key])
                    .await
                    .context("failed to delete stale lock")?;
                tx.execute(
                    "INSERT INTO locks (key, holder, acquired_at) VALUES (?1, ?2, ?3)",
                    turso::params![key, holder, now],
                )
                .await
                .context("failed to insert lock row after stealing")?;
                true
            }
            // Still held and fresh, or was released between our failed
            // insert above and this check -- either way, not ours yet.
            // The latter case just costs one extra poll cycle.
            _ => false,
        };
        tx.commit()
            .await
            .context("failed to commit lock-steal transaction")?;
        Ok(stolen)
    }

    async fn release(&self, key: &str) {
        if let Err(e) = self
            .conn
            .lock()
            .await
            .execute("DELETE FROM locks WHERE key = ?1", turso::params![key])
            .await
        {
            tracing::warn!("failed to release lock for {key}: {e:#}");
        }
    }

    /// Acquire the cross-process lock for `key`, blocking until it's free.
    /// Multiple server instances can share one content directory; this
    /// keeps two of them from verifying/downloading the same depot's files
    /// at the same time. No more `spawn_blocking` bridging needed here --
    /// turso's calls are natively async, unlike rusqlite's.
    pub async fn acquire_lock(self: &Arc<Self>, key: String) -> Result<LockGuard> {
        let holder = holder_id();
        let mut waited = false;
        loop {
            if self.try_acquire(&key, &holder).await? {
                if waited {
                    tracing::info!("[{key}] acquired lock (was waiting on another process)");
                }
                return Ok(LockGuard {
                    state: self.clone(),
                    key,
                });
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
        // release() is async (turso has no sync API), but Drop can't be --
        // spawn it as a detached task instead of blocking. This is a real,
        // deliberate behavior change from the old rusqlite version (which
        // released synchronously, inline, on drop): the lock row deletion
        // now happens shortly *after* the guard is dropped, not atomically
        // with it. Harmless for correctness (the DB's own locking is what
        // actually serializes acquirers, and a lock that outlives its
        // guard by a few milliseconds just delays the next acquirer, same
        // as the existing STALE_LOCK_SECS fallback already tolerates for a
        // crashed holder) -- just worth knowing it's no longer synchronous.
        //
        // Guarded with try_current() rather than a bare spawn(): dropping
        // outside a tokio runtime (e.g. during shutdown after the runtime
        // has already stopped) would otherwise panic. Falls back to
        // relying on STALE_LOCK_SECS's eventual steal instead.
        let state = self.state.clone();
        let key = self.key.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                state.release(&key).await;
            });
        } else {
            tracing::warn!(
                "[{key}] LockGuard dropped outside a tokio runtime -- lock will only clear via the stale-lock timeout"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn open_test_state() -> (TempDir, Arc<SyncState>) {
        let dir = TempDir::new().expect("failed to create temp dir");
        let state = SyncState::open(dir.path())
            .await
            .expect("failed to open test SyncState");
        (dir, state)
    }

    #[tokio::test]
    async fn open_initializes_schema_idempotently() {
        let (dir, _state) = open_test_state().await;
        // Re-opening against the same directory (CREATE TABLE IF NOT EXISTS)
        // must not error.
        SyncState::open(dir.path())
            .await
            .expect("re-open should be idempotent");
    }

    #[tokio::test]
    async fn source_upsert_set_delete_round_trip() {
        let (_dir, state) = open_test_state().await;

        state
            .upsert_source("src1", &[1, 2, 3])
            .await
            .expect("upsert_source failed");
        assert_eq!(
            state
                .candidate_ids_for_source("src1")
                .await
                .expect("candidate_ids_for_source failed"),
            Some(vec![1, 2, 3])
        );

        state
            .set_source_mods("src1", &[10, 20])
            .await
            .expect("set_source_mods failed");
        let mut mod_ids = state
            .mod_ids_for_source("src1")
            .await
            .expect("mod_ids_for_source failed");
        mod_ids.sort_unstable();
        assert_eq!(mod_ids, vec![10, 20]);

        let mut desired = state
            .desired_mod_ids()
            .await
            .expect("desired_mod_ids failed");
        desired.sort_unstable();
        assert_eq!(desired, vec![10, 20]);

        // Replacing membership drops what's no longer wanted and keeps
        // what still is.
        state
            .set_source_mods("src1", &[20, 30])
            .await
            .expect("set_source_mods (replace) failed");
        let mut mod_ids = state
            .mod_ids_for_source("src1")
            .await
            .expect("mod_ids_for_source (after replace) failed");
        mod_ids.sort_unstable();
        assert_eq!(mod_ids, vec![20, 30]);

        state
            .delete_source("src1")
            .await
            .expect("delete_source failed");
        assert_eq!(
            state
                .candidate_ids_for_source("src1")
                .await
                .expect("candidate_ids_for_source (after delete) failed"),
            None
        );
        assert!(
            state
                .mod_ids_for_source("src1")
                .await
                .expect("mod_ids_for_source (after delete) failed")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn mark_synced_and_is_synced_round_trip() {
        let (_dir, state) = open_test_state().await;

        assert_eq!(state.last_manifest_id("depot/1").await, None);
        assert!(!state.is_synced("depot/1", 42, true).await);

        state.mark_synced("depot/1", 42, 1024).await;
        assert_eq!(state.last_manifest_id("depot/1").await, Some(42));
        assert!(state.is_synced("depot/1", 42, true).await);
        // A dir that no longer exists is never "synced", regardless of
        // the recorded manifest_id.
        assert!(!state.is_synced("depot/1", 42, false).await);
        // A manifest_id mismatch (content changed) is a cache miss too.
        assert!(!state.is_synced("depot/1", 43, true).await);

        state.invalidate("depot/1").await;
        assert_eq!(state.last_manifest_id("depot/1").await, None);
    }

    #[tokio::test]
    async fn acquire_lock_serializes_concurrent_acquirers() {
        let (_dir, state) = open_test_state().await;

        let guard = state
            .acquire_lock("key1".to_string())
            .await
            .expect("first acquire_lock failed");

        // A second acquirer must not get it while the first still holds
        // it -- try_acquire directly (not acquire_lock, which would just
        // block forever polling).
        assert!(
            !state
                .try_acquire("key1", "other-holder")
                .await
                .expect("try_acquire failed")
        );

        drop(guard);
        // LockGuard's release is spawned, not synchronous -- give it a
        // moment to actually run before checking it cleared.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            state
                .try_acquire("key1", "other-holder")
                .await
                .expect("try_acquire after release failed")
        );
    }

    #[tokio::test]
    async fn stale_lock_gets_stolen() {
        let (_dir, state) = open_test_state().await;

        // Seed a lock row backdated well past STALE_LOCK_SECS, simulating
        // a holder that crashed without ever releasing.
        let stale_at = now_unix() - STALE_LOCK_SECS - 60;
        state
            .conn
            .lock()
            .await
            .execute(
                "INSERT INTO locks (key, holder, acquired_at) VALUES (?1, ?2, ?3)",
                turso::params!["key1", "dead-holder", stale_at],
            )
            .await
            .expect("failed to seed stale lock");

        assert!(
            state
                .try_acquire("key1", "new-holder")
                .await
                .expect("try_acquire (steal) failed"),
            "a sufficiently stale lock should be stolen"
        );
    }

    #[tokio::test]
    async fn fresh_lock_is_not_stolen() {
        let (_dir, state) = open_test_state().await;

        state
            .conn
            .lock()
            .await
            .execute(
                "INSERT INTO locks (key, holder, acquired_at) VALUES (?1, ?2, ?3)",
                turso::params!["key1", "live-holder", now_unix()],
            )
            .await
            .expect("failed to seed fresh lock");

        assert!(
            !state
                .try_acquire("key1", "new-holder")
                .await
                .expect("try_acquire (should not steal) failed"),
            "a fresh lock must not be stolen"
        );
    }

    /// The actual bug this Mutex fixes: two tasks calling methods that hit
    /// the same underlying turso session concurrently used to fail with
    /// `Misuse("concurrent use forbidden")` before this Mutex existed --
    /// confirmed live in production (two ModSources' auto-sync tasks
    /// racing `acquire_lock`). Exercises that same shape: two concurrent
    /// callers hitting the connection at once.
    #[tokio::test]
    async fn concurrent_operations_do_not_error() {
        let (_dir, state) = open_test_state().await;

        let a = state.clone();
        let b = state.clone();
        let (ra, rb) = tokio::join!(
            async move { a.upsert_source("src-a", &[1, 2]).await },
            async move { b.upsert_source("src-b", &[3, 4]).await },
        );
        ra.expect("concurrent upsert_source (a) failed");
        rb.expect("concurrent upsert_source (b) failed");
    }
}
