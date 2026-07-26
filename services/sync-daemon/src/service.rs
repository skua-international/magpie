use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use protocol::proto::sync::v1::{
    DeregisterSourceRequest, DeregisterSourceResponse, GetSourceModsRequest, GetSourceModsResponse,
    GetSyncStatsRequest, GetSyncStatsResponse, GetSyncStatusRequest, GetSyncStatusResponse,
    GetSyncedModRequest, GetSyncedModResponse, InvalidateModRequest, InvalidateModResponse,
    ListSyncedModsRequest, ListSyncedModsResponse, RefreshSourceRequest, RefreshSourceResponse,
    RefreshSteamAuthRequest, RefreshSteamAuthResponse, RegisterSourceRequest,
    RegisterSourceResponse, ResolvedMod as ProtoResolvedMod, SyncContentRequest,
    SyncContentResponse, SyncService, SyncedMod,
};
use steam_sync::cache::SyncState;
use steam_sync::steam::{self, CmPool, ResolvedMod, SyncTasks};
use steam_sync::workshop;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::secrets::{self, Session};

/// Shared state, `Arc`-wrapped so it can be cloned cheaply into a spawned
/// background sync task -- kept separate from [`SyncServiceImpl`] (which
/// only borrows `&self` per the trait's method signatures) specifically
/// so spawning doesn't need a self-referential `Arc<Self>`.
pub struct Shared {
    /// `None` if no Steam session has ever been established (no stored
    /// session Secret, no STEAM_USER/STEAM_PASSWORD configured) or the
    /// last `CmPool::start` attempt failed -- every RPC that needs Steam
    /// access returns a clear precondition-failed error in that case
    /// rather than panicking or hanging, and RefreshSteamAuth is the way
    /// out. A plain field, not something RefreshSteamAuth replaces in
    /// place -- it persists a freshly established session to the Secret
    /// and exits the process, letting a restart pick it up fresh (see
    /// that handler's own doc for why), rather than hot-swapping this.
    pub pool: Option<Arc<CmPool>>,
    pub sync_state: Arc<SyncState>,
    pub content_root: PathBuf,
    /// See steam::DEFAULT_DOWNLOAD_WORKERS' own doc -- chunk-level
    /// concurrency within each depot/mod's own verify+download pass.
    pub download_workers: usize,
    pub client: kube::Client,
    pub namespace: String,
    pub steam_session_secret_name: String,
    /// Count of `sync_content` calls currently in flight -- a counter, not
    /// a bool, since this can be entered from three independent places
    /// (the `SyncContent` RPC, the reconciler's auto-sync-on-first-resolve,
    /// main.rs's sync-on-startup) with no guarantee they're ever mutually
    /// exclusive. `GetSyncStatus`'s `syncing` field is just `> 0`.
    syncing: AtomicUsize,
}

// THROWAWAY -- not committed. mi_collect isn't exposed by either the
// `mimalloc` or `libmimalloc-sys` crates (only basic alloc/free are
// bound), but the C library is already statically linked in via
// libmimalloc-sys's build script, so the symbol exists in the final
// binary regardless -- declaring it ourselves is enough to call it.
// Gated out of the dhat-heap profiling build, which uses a different
// global allocator entirely (calling into mimalloc's collector when
// mimalloc was never actually installed as the allocator would be
// undefined behavior).
#[cfg(not(feature = "dhat-heap"))]
unsafe extern "C" {
    fn mi_collect(force: bool);
}

/// RAII guard incrementing `Shared::syncing` on creation, decrementing on
/// drop -- so a `sync_content` call that returns early via `?` still
/// clears itself, no separate cleanup needed at every return point.
/// Also the trigger point for an explicit mimalloc purge: once the last
/// in-flight sync finishes (counter back to 0), force-collect rather than
/// waiting on mimalloc's own time-decayed background purge, which
/// benchmarking showed plateaus well above genuinely-live memory for this
/// workload's large one-shot decompression buffers.
struct SyncingGuard<'a>(&'a AtomicUsize);

impl<'a> SyncingGuard<'a> {
    fn enter(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for SyncingGuard<'_> {
    fn drop(&mut self) {
        let previous = self.0.fetch_sub(1, Ordering::SeqCst);
        #[cfg(not(feature = "dhat-heap"))]
        if previous == 1 {
            info!("last in-flight sync finished, forcing mimalloc collect");
            unsafe {
                mi_collect(true);
            }
        }
        #[cfg(feature = "dhat-heap")]
        let _ = previous;
    }
}

impl Shared {
    pub fn new(
        pool: Option<Arc<CmPool>>,
        sync_state: Arc<SyncState>,
        content_root: PathBuf,
        client: kube::Client,
        namespace: String,
        steam_session_secret_name: String,
        download_workers: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            sync_state,
            content_root,
            client,
            namespace,
            steam_session_secret_name,
            download_workers,
            syncing: AtomicUsize::new(0),
        })
    }

    /// The active connection pool, or a clear precondition-failed error if
    /// no Steam session has been established yet -- every RPC needing
    /// Steam access should go through this instead of touching `self.pool`
    /// directly, so that error is consistent everywhere.
    fn pool(&self) -> anyhow::Result<&Arc<CmPool>> {
        self.pool.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no Steam session established -- call RefreshSteamAuth first")
        })
    }

    /// Resolve `candidate_ids` and persist the result as `source_id`'s
    /// current membership. Shared by the `RegisterSource` RPC and the
    /// background poller (main.rs), which both need exactly this
    /// resolve-then-diff-upsert sequence.
    pub async fn register_source_impl(
        &self,
        candidate_ids: &[u64],
        source_id: &str,
    ) -> anyhow::Result<RegisterSourceOutcome> {
        let mut conn = self.pool()?.acquire().await;
        let result = steam::resolve_source_ids(&mut conn, candidate_ids).await;
        if let Err(e) = &result {
            if steam::is_transient(e) {
                conn.mark_bad();
            }
        }
        drop(conn);
        let outcome = result?;

        let mod_ids: Vec<u64> = outcome.mods.iter().map(|m| m.mod_id).collect();
        self.sync_state
            .upsert_source(source_id, candidate_ids)
            .await?;
        self.sync_state.set_source_mods(source_id, &mod_ids).await?;
        for m in &outcome.mods {
            self.sync_state.record_mod_title(m.mod_id, &m.title).await;
        }

        let (root_title, root_is_collection) = match candidate_ids {
            [single] => (
                outcome
                    .candidate_titles
                    .get(single)
                    .cloned()
                    .unwrap_or_default(),
                outcome.candidate_is_collection.get(single).copied(),
            ),
            _ => (String::new(), None),
        };

        Ok(RegisterSourceOutcome {
            mods: outcome.mods,
            root_title,
            root_is_collection,
        })
    }

    /// Re-resolve `source_id` against whatever candidate IDs it was last
    /// registered with -- the on-demand counterpart to the background
    /// poller's full sweep, for a caller (the controller's
    /// UpdateServer/StartServer) that wants one specific source refreshed
    /// right now without having to remember/resupply its candidate IDs.
    pub async fn refresh_source_impl(
        &self,
        source_id: &str,
    ) -> anyhow::Result<RegisterSourceOutcome> {
        let candidate_ids = self
            .sync_state
            .candidate_ids_for_source(source_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown source: {source_id}"))?;
        self.register_source_impl(&candidate_ids, source_id).await
    }

    /// Downloads server/CDLC depots plus every currently-registered
    /// source's resolved mods into the shared golden tree. Every
    /// ArmaServer's own content comes from a read-only btrfs snapshot of
    /// this tree (services/magpie-csi's NodeStageVolume, mode: snapshot)
    /// taken whenever its PVC is created -- this only has to keep the
    /// golden tree itself current, nothing here produces or tracks a
    /// claim/snapshot of its own anymore. Called from three places: the
    /// `SyncContent` RPC (spawned, not awaited -- see that handler),
    /// the reconciler's auto-sync-on-first-resolve (`reconcile.rs`), and
    /// main.rs's sync-on-startup.
    pub async fn sync_content(&self) -> anyhow::Result<()> {
        let _guard = SyncingGuard::enter(&self.syncing);
        let sem = Arc::new(Semaphore::new(steam::SYNC_CONCURRENCY));
        let tasks: Mutex<SyncTasks> = Mutex::new(SyncTasks::new());

        let pool = self.pool()?;
        // Server/CDLC depots need a licensed (authenticated) session --
        // anonymous can't reach them at all (see login_or_degrade_to_anonymous
        // in steam-sync). Checking any_authenticated() first avoids
        // hammering Steam with a doomed request when every slot has
        // degraded, and -- just as importantly -- a failure here no longer
        // aborts the whole sync_content call via `?`: mod syncing below is
        // independent and should still proceed even if server content is
        // currently unreachable.
        if pool.any_authenticated() {
            let mut conn = pool.acquire().await;
            let result = steam::resolve_and_spawn_server(
                &mut conn,
                &self.content_root,
                false,
                &[],
                sem.clone(),
                &tasks,
                self.sync_state.clone(),
                self.download_workers,
            )
            .await;
            if let Err(e) = &result {
                if steam::is_transient(e) {
                    conn.mark_bad();
                }
            }
            drop(conn);
            if let Err(e) = result {
                warn!("server/CDLC content sync failed, will retry next cycle: {e:#}");
            }
        } else {
            warn!(
                "skipping server/CDLC content sync -- no authenticated Steam session in the pool (running anonymous), only workshop mods will sync"
            );
        }

        let desired = self.sync_state.desired_mod_ids().await?;
        if !desired.is_empty() {
            let mut conn = self.pool()?.acquire().await;
            let result = workshop::sync_mods(
                &mut conn,
                &desired,
                false,
                &self.content_root,
                sem.clone(),
                &tasks,
                self.sync_state.clone(),
                self.download_workers,
            )
            .await;
            if let Err(e) = &result {
                if steam::is_transient(e) {
                    conn.mark_bad();
                }
            }
            drop(conn);
            result?;
        }

        let mut tasks = tasks.into_inner().unwrap();
        while let Some(result) = tasks.join_next().await {
            result??;
        }
        Ok(())
    }
}

pub struct RegisterSourceOutcome {
    pub mods: Vec<ResolvedMod>,
    pub root_title: String,
    /// True when `candidate_ids` was a single id and that id is itself a
    /// pure Steam Workshop collection -- `None` for a multi-candidate
    /// source (e.g. a preset), where "is this one thing a collection"
    /// doesn't apply. See `steam::ResolveOutcome::candidate_is_collection`
    /// for why this can't just be inferred from whether `mods` changed
    /// shape relative to `candidate_ids`.
    pub root_is_collection: Option<bool>,
}

pub struct SyncServiceImpl {
    pub shared: Arc<Shared>,
}

impl SyncServiceImpl {
    pub fn new(shared: Arc<Shared>) -> Arc<Self> {
        Arc::new(Self { shared })
    }
}

impl SyncService for SyncServiceImpl {
    async fn register_source<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, RegisterSourceRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<RegisterSourceResponse> + Send + use<'a>> {
        let candidate_ids: Vec<u64> = request.candidate_ids.iter().copied().collect();
        let source_id = request.source_id.to_string();

        let outcome = self
            .shared
            .register_source_impl(&candidate_ids, &source_id)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;

        let mods = outcome
            .mods
            .into_iter()
            .map(|m| ProtoResolvedMod {
                mod_id: m.mod_id,
                title: m.title,
                ..Default::default()
            })
            .collect();
        Response::ok(RegisterSourceResponse {
            mods,
            root_title: outcome.root_title,
            ..Default::default()
        })
    }

    async fn deregister_source<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, DeregisterSourceRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<DeregisterSourceResponse> + Send + use<'a>> {
        self.shared
            .sync_state
            .delete_source(&request.source_id)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(DeregisterSourceResponse::default())
    }

    async fn sync_content<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, SyncContentRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<SyncContentResponse> + Send + use<'a>> {
        let shared = self.shared.clone();
        tokio::spawn(async move {
            info!("syncing content (SyncContent RPC)");
            match shared.sync_content().await {
                Ok(()) => info!("content synced"),
                Err(e) => warn!("failed to sync content: {e:#}"),
            }
        });
        Response::ok(SyncContentResponse::default())
    }

    async fn get_source_mods<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetSourceModsRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetSourceModsResponse> + Send + use<'a>> {
        let mod_ids = self
            .shared
            .sync_state
            .mod_ids_for_source(request.source_id)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(GetSourceModsResponse {
            mod_ids,
            ..Default::default()
        })
    }

    async fn refresh_source<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, RefreshSourceRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<RefreshSourceResponse> + Send + use<'a>> {
        let outcome = self
            .shared
            .refresh_source_impl(request.source_id)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        let mods = outcome
            .mods
            .into_iter()
            .map(|m| ProtoResolvedMod {
                mod_id: m.mod_id,
                title: m.title,
                ..Default::default()
            })
            .collect();
        Response::ok(RefreshSourceResponse {
            mods,
            ..Default::default()
        })
    }

    async fn list_synced_mods<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListSyncedModsRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListSyncedModsResponse> + Send + use<'a>> {
        let mods = self
            .shared
            .sync_state
            .list_synced_mods()
            .await
            .into_iter()
            .map(|m| SyncedMod {
                mod_id: m.mod_id,
                manifest_id: m.manifest_id,
                size_bytes: m.size_bytes,
                title: m.title,
                ..Default::default()
            })
            .collect();
        Response::ok(ListSyncedModsResponse {
            mods,
            ..Default::default()
        })
    }

    async fn get_synced_mod<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetSyncedModRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetSyncedModResponse> + Send + use<'a>> {
        let mod_id = request.mod_id;
        let m = self
            .shared
            .sync_state
            .get_synced_mod(mod_id)
            .await
            .map(|m| SyncedMod {
                mod_id: m.mod_id,
                manifest_id: m.manifest_id,
                size_bytes: m.size_bytes,
                title: m.title,
                ..Default::default()
            });
        let source_ids = self.shared.sync_state.sources_for_mod(mod_id).await;
        Response::ok(GetSyncedModResponse {
            r#mod: m.into(),
            source_ids,
            ..Default::default()
        })
    }

    async fn get_sync_stats<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetSyncStatsRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetSyncStatsResponse> + Send + use<'a>> {
        Response::ok(GetSyncStatsResponse {
            mods_bytes: self.shared.sync_state.total_mods_size().await,
            game_files_bytes: self.shared.sync_state.total_game_files_size().await,
            ..Default::default()
        })
    }

    async fn get_sync_status<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetSyncStatusRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetSyncStatusResponse> + Send + use<'a>> {
        Response::ok(GetSyncStatusResponse {
            syncing: self.shared.syncing.load(Ordering::SeqCst) > 0,
            game_files_ready: self.shared.sync_state.total_game_files_size().await > 0,
            ..Default::default()
        })
    }

    async fn invalidate_mod<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, InvalidateModRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<InvalidateModResponse> + Send + use<'a>> {
        // Matches list_synced_mods'/sync_key's own key format: every Arma 3
        // workshop item shares depot_id (consumer_appid) 107410.
        let key = format!("107410/{}", request.mod_id);
        self.shared.sync_state.invalidate(&key).await;
        Response::ok(InvalidateModResponse::default())
    }

    /// `request.refresh_token` must already be negotiated -- this process
    /// (like every other deployed service) never sees a Steam password,
    /// not even transiently. The interactive username+password (+ Guard
    /// code) negotiation happens entirely client-side, in
    /// `magpiectl admin refresh-steam-auth`, which calls this RPC with
    /// only the result. See the proto's own doc for the full rationale.
    async fn refresh_steam_auth<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, RefreshSteamAuthRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<RefreshSteamAuthResponse> + Send + use<'a>> {
        let session = Session {
            user: request.username.to_string(),
            refresh_token: request.refresh_token.to_string(),
        };
        secrets::write_session(
            &self.shared.client,
            &self.shared.namespace,
            &self.shared.steam_session_secret_name,
            &session,
        )
        .await
        .map_err(|e| {
            ConnectError::internal(format!("failed to persist new Steam session: {e:#}"))
        })?;

        // The freshly established session isn't picked up by the
        // already-running CmPool (if any) -- exiting and letting the
        // Deployment restart this Pod is far simpler and safer than
        // hot-swapping an in-flight connection pool's auth in place (a
        // real class of concurrency bugs for very little benefit here,
        // since establishing a new session is already an infrequent,
        // deliberate admin action, not something latency-sensitive).
        // Spawned with a short delay so this response actually reaches
        // the caller before the process exits, rather than racing the
        // connection closing against the response being flushed.
        info!("new Steam session established, restarting to pick it up");
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            std::process::exit(0);
        });

        Response::ok(RefreshSteamAuthResponse::default())
    }
}
