use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use protocol::proto::sync::v1::{
    BeginQrLoginRequest, BeginQrLoginResponse, CollectionSummary, DeleteCollectionRequest,
    DeleteCollectionResponse, DeregisterSourceRequest, DeregisterSourceResponse,
    GetCollectionRequest, GetCollectionResponse, GetSourceModsRequest, GetSourceModsResponse,
    GetSyncStatsRequest, GetSyncStatsResponse, GetSyncStatusRequest, GetSyncStatusResponse,
    GetSyncedModRequest, GetSyncedModResponse, InvalidateModRequest, InvalidateModResponse,
    ListOwnedCollectionsRequest, ListOwnedCollectionsResponse, ListSyncedModsRequest,
    ListSyncedModsResponse, PollQrLoginRequest, PollQrLoginResponse, PublishCollectionRequest,
    PublishCollectionResponse, RefreshSourceRequest, RefreshSourceResponse,
    RefreshSteamAuthRequest, RefreshSteamAuthResponse, RegisterSourceRequest,
    RegisterSourceResponse, ResolveWorkshopItemsRequest, ResolveWorkshopItemsResponse,
    ResolvedMod as ProtoResolvedMod, SyncContentRequest, SyncContentResponse, SyncService,
    SyncedMod, WorkshopItem,
};
use steam_sync::cache::SyncState;
use steam_sync::collection::{self, Visibility};
use steam_sync::steam::{self, CmPool, ResolvedMod, SyncTasks};
use steam_sync::workshop;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::capacity::CapacityClient;
use crate::secrets::{self, Session};
use steam_sync::capacity::CapacityReserver;

/// Shared state, `Arc`-wrapped so it can be cloned cheaply into a spawned
/// background sync task -- kept separate from [`SyncServiceImpl`] (which
/// only borrows `&self` per the trait's method signatures) specifically
/// so spawning doesn't need a self-referential `Arc<Self>`.
/// State of one in-flight QR login, keyed by the session id handed back
/// to the caller.
///
/// Needed because `steam::poll_qr_login` blocks until the Steam mobile
/// app confirms and consumes the session doing it -- neither of which
/// suits a unary RPC a browser polls. `BeginQrLogin` therefore spawns a
/// task that does the blocking wait and records the outcome here, and
/// `PollQrLogin` just reads this map and returns immediately.
///
/// In-process, so both calls must reach the same replica. sync-daemon
/// already runs as a single replica; this is the same constraint
/// services/identity carries for its exchange-code map.
pub enum QrLoginState {
    Pending,
    Confirmed { username: String },
    Failed { error: String },
}

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
    /// In-flight QR logins -- see `QrLoginState`. A std Mutex, not tokio's:
    /// every access is a map read/write with no await held across it.
    pub qr_logins: Arc<std::sync::Mutex<std::collections::HashMap<String, QrLoginState>>>,
    pub sync_state: Arc<SyncState>,
    pub content_root: PathBuf,
    /// See steam::DEFAULT_DOWNLOAD_WORKERS' own doc -- chunk-level
    /// concurrency within each depot/mod's own verify+download pass.
    pub download_workers: usize,
    pub client: kube::Client,
    pub namespace: String,
    pub steam_session_secret_name: String,
    /// Announces an upcoming batch's byte total to magpie-csi so the
    /// blob is grown before the writes start, rather than after its
    /// watchdog notices. `None` when CSI_CAPACITY_URL isn't set (or the
    /// client failed to build), in which case that watchdog is the only
    /// defense -- which is what shipped before this existed.
    pub capacity: Option<Arc<CapacityClient>>,
    /// Count of `sync_content` calls currently in flight -- a counter, not
    /// a bool, since this can be entered from three independent places
    /// (the `SyncContent` RPC, the reconciler's auto-sync-on-first-resolve,
    /// main.rs's sync-on-startup) with no guarantee they're ever mutually
    /// exclusive. `GetSyncStatus`'s `syncing` field is just `> 0`.
    syncing: AtomicUsize,
}

impl Shared {
    /// Depot/mod syncs currently running -- the same counter
    /// `GetSyncStatus` reports, exposed so the metric and the RPC can
    /// never disagree about it.
    pub fn in_flight(&self) -> usize {
        self.syncing.load(Ordering::Relaxed)
    }
}

/// RAII guard incrementing `Shared::syncing` on creation, decrementing on
/// drop -- so a `sync_content` call that returns early via `?` still
/// clears itself, no separate cleanup needed at every return point.
struct SyncingGuard<'a>(&'a AtomicUsize);

impl<'a> SyncingGuard<'a> {
    fn enter(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for SyncingGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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
        capacity: Option<Arc<CapacityClient>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            qr_logins: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            sync_state,
            content_root,
            client,
            namespace,
            steam_session_secret_name,
            download_workers,
            capacity,
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

    /// Run `f` on a pooled connection, marking it bad on a transient
    /// failure exactly as `register_source_impl` does.
    async fn with_conn<T>(
        &self,
        f: impl AsyncFnOnce(&mut steam::CmConnection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut conn = self.pool()?.acquire().await;
        let result = f(&mut conn).await;
        if let Err(e) = &result
            && steam::is_transient(e)
        {
            conn.mark_bad();
        }
        result
    }

    /// [`Self::with_conn`], additionally handing `f` the SteamID64 the
    /// connection is logged in as and refusing an anonymous one -- for
    /// anything done *as* the cluster's account. See
    /// [`collection::account_steam_id`].
    async fn with_account<T>(
        &self,
        f: impl AsyncFnOnce(&mut steam::CmConnection, u64) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.with_conn(async |conn| {
            let steam_id = collection::account_steam_id(conn)?;
            f(conn, steam_id).await
        })
        .await
    }

    /// Resolve `candidate_ids` into the flat, ordered mod list a
    /// collection would hold, without registering or publishing anything.
    pub async fn resolve_items_impl(
        &self,
        candidate_ids: &[u64],
    ) -> anyhow::Result<steam::ResolveOutcome> {
        self.with_conn(async |conn| steam::resolve_source_ids(conn, candidate_ids).await)
            .await
    }

    pub async fn list_owned_collections_impl(
        &self,
    ) -> anyhow::Result<Vec<collection::CollectionSummary>> {
        self.with_account(async |conn, steam_id| {
            collection::list_owned_collections(conn, steam_id).await
        })
        .await
    }

    /// One collection, its members resolved, and whether the cluster's
    /// account owns it.
    pub async fn get_collection_impl(&self, collection_id: u64) -> anyhow::Result<CollectionView> {
        self.with_account(async |conn, steam_id| {
            let details = collection::get_collection_details(conn, collection_id).await?;
            let resolved = steam::resolve_source_ids(conn, &details.children).await?;
            Ok(CollectionView {
                owned: details.creator == steam_id,
                details,
                resolved,
            })
        })
        .await
    }

    /// Resolve `candidate_ids` and publish them as a new collection, or
    /// replace an owned one's metadata and membership with them.
    ///
    /// Deliberately does not touch `sync_state`: publishing a collection
    /// is not registering a mod source, and a collection created here
    /// starts syncing only if someone separately registers it. Keeping
    /// the two apart is what stops "make me a shareable link" from
    /// quietly enlisting the cluster into downloading its contents.
    pub async fn publish_collection_impl(
        &self,
        existing_collection_id: Option<u64>,
        title: &str,
        description: &str,
        visibility: Visibility,
        candidate_ids: &[u64],
    ) -> anyhow::Result<PublishOutcome> {
        self.with_account(async |conn, steam_id| {
            let resolved = steam::resolve_source_ids(conn, candidate_ids).await?;
            let mod_ids: Vec<u64> = resolved.mods.iter().map(|m| m.mod_id).collect();
            // Resolution drops private/removed items with a warning rather
            // than failing, so an entirely unresolvable input arrives here
            // as an empty list. Publishing an empty collection (or wiping
            // an existing one's membership) is never what was meant.
            if mod_ids.is_empty() {
                anyhow::bail!(
                    "none of the {} requested ids resolved to a mod -- nothing to put in a \
                     collection",
                    candidate_ids.len()
                );
            }

            let (collection_id, created) = match existing_collection_id {
                Some(id) => {
                    let details = collection::get_collection_details(conn, id).await?;
                    ensure_owned(&details, steam_id)?;
                    // Membership first: it's the part that can be refused
                    // for content reasons, and failing it shouldn't leave
                    // the collection renamed to describe a list it
                    // doesn't hold.
                    collection::set_collection_children(conn, id, &mod_ids).await?;
                    collection::update_collection(conn, id, title, description, visibility).await?;
                    (id, false)
                }
                None => {
                    let id = collection::publish_collection(conn, title, description, visibility)
                        .await?;
                    if let Err(e) = collection::set_collection_children(conn, id, &mod_ids).await {
                        // Don't leave an empty collection behind under the
                        // account's name for a publish that, as far as the
                        // caller can tell, failed.
                        if let Err(cleanup) = collection::delete_collection(conn, id).await {
                            warn!(
                                "collection {id} was published but populating it failed, and \
                                 deleting it again also failed -- it is left empty: {cleanup:#}"
                            );
                        }
                        return Err(e);
                    }
                    (id, true)
                }
            };

            Ok(PublishOutcome {
                collection_id,
                created,
                mods: resolved.mods,
                unresolved: resolved.unresolved,
            })
        })
        .await
    }

    pub async fn delete_collection_impl(&self, collection_id: u64) -> anyhow::Result<()> {
        self.with_account(async |conn, steam_id| {
            let details = collection::get_collection_details(conn, collection_id).await?;
            ensure_owned(&details, steam_id)?;
            collection::delete_collection(conn, collection_id).await
        })
        .await
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
        // Timed around the whole pass rather than per depot: this is the
        // number an operator actually asks about ("how long does a full
        // resync take"), and it is the one a load test was hand-timing
        // with a shell loop before this existed.
        let started = std::time::Instant::now();
        // Both download paths below share this: each announces its own
        // batch total under its own key, so the two sum on the far side
        // rather than overwriting each other.
        let reserver = self
            .capacity
            .as_deref()
            .map(|c| c as &dyn steam_sync::capacity::CapacityReserver);
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
                reserver,
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
                reserver,
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

        // Everything above has finished writing, so hand the headroom
        // back rather than leaving it held until the TTL lapses -- while
        // a reservation stands, magpie-csi won't shrink the blob even if
        // the content that justified it was since deleted.
        if let Some(capacity) = &self.capacity {
            capacity
                .release(steam_sync::capacity::KEY_SERVER_DEPOTS)
                .await;
            capacity.release(steam_sync::capacity::KEY_WORKSHOP).await;
        }

        // Recorded on the success path only: a sync that failed partway
        // has a duration, but not one comparable to a completed pass, and
        // mixing them makes the histogram describe neither.
        crate::metrics::sync_duration().record(started.elapsed().as_secs_f64(), &[]);
        Ok(())
    }
}

/// Refuse to edit or delete a collection someone else published.
///
/// Steam would refuse too, but with a bare eresult; this names the actual
/// problem. Checked by creator rather than left to Steam also means an
/// account with elevated Workshop rights couldn't be talked into editing a
/// collection it merely has access to.
fn ensure_owned(details: &collection::CollectionDetails, steam_id: u64) -> anyhow::Result<()> {
    if details.creator != steam_id {
        anyhow::bail!(
            "collection {} ({:?}) was published by another Steam account, not the cluster's -- \
             it can be copied into a new collection, but not changed",
            details.id,
            details.title
        );
    }
    Ok(())
}

pub struct CollectionView {
    pub details: collection::CollectionDetails,
    pub owned: bool,
    /// `details.children`, resolved.
    pub resolved: steam::ResolveOutcome,
}

pub struct PublishOutcome {
    pub collection_id: u64,
    /// True when this call published the collection itself, false when it
    /// only replaced an existing one's metadata and membership.
    pub created: bool,
    pub mods: Vec<ResolvedMod>,
    pub unresolved: Vec<u64>,
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

    async fn resolve_workshop_items<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, ResolveWorkshopItemsRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ResolveWorkshopItemsResponse> + Send + use<'a>>
    {
        let candidate_ids: Vec<u64> = request.candidate_ids.iter().copied().collect();
        if candidate_ids.is_empty() {
            return Err(ConnectError::invalid_argument(
                "candidate_ids must not be empty",
            ));
        }
        let outcome = self
            .shared
            .resolve_items_impl(&candidate_ids)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(ResolveWorkshopItemsResponse {
            mods: to_proto_items(outcome.mods),
            unresolved: outcome.unresolved,
            ..Default::default()
        })
    }

    async fn list_owned_collections<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListOwnedCollectionsRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListOwnedCollectionsResponse> + Send + use<'a>>
    {
        let collections = self
            .shared
            .list_owned_collections_impl()
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?
            .into_iter()
            .map(|c| CollectionSummary {
                id: c.id,
                title: c.title,
                visibility: proto_visibility(c.visibility),
                item_count: c.item_count,
                updated_at_unix_ms: unix_ms(c.time_updated),
                ..Default::default()
            })
            .collect();
        Response::ok(ListOwnedCollectionsResponse {
            collections,
            ..Default::default()
        })
    }

    async fn get_collection<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetCollectionRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetCollectionResponse> + Send + use<'a>> {
        let collection_id = required_collection_id(request.collection_id)?;
        let view = self
            .shared
            .get_collection_impl(collection_id)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(GetCollectionResponse {
            id: view.details.id,
            title: view.details.title,
            description: view.details.description,
            visibility: proto_visibility(view.details.visibility),
            owned: view.owned,
            updated_at_unix_ms: unix_ms(view.details.time_updated),
            mods: to_proto_items(view.resolved.mods),
            unresolved: view.resolved.unresolved,
            ..Default::default()
        })
    }

    async fn publish_collection<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, PublishCollectionRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<PublishCollectionResponse> + Send + use<'a>> {
        // Rejected rather than defaulted: the proto's zero value is
        // UNSPECIFIED precisely so a caller that forgot the field cannot
        // land on PUBLIC by accident. The default belongs in the UI,
        // where a person can see it. `to_i32` rather than matching on the
        // known variants: a value this build doesn't recognise stays a
        // number and is refused by from_proto, instead of a match arm
        // having to invent a fallback for it.
        let visibility = Visibility::from_proto(request.visibility.to_i32())
            .map_err(|e| ConnectError::invalid_argument(format!("{e:#}")))?;
        if request.title.trim().is_empty() {
            return Err(ConnectError::invalid_argument("a collection needs a title"));
        }
        let candidate_ids: Vec<u64> = request.candidate_ids.iter().copied().collect();
        if candidate_ids.is_empty() {
            return Err(ConnectError::invalid_argument(
                "candidate_ids must not be empty",
            ));
        }
        // 0 is proto3's absent uint64, so it reads as "publish a new one"
        // rather than as a collection id.
        let existing = Some(request.collection_id).filter(|id| *id != 0);

        let outcome = self
            .shared
            .publish_collection_impl(
                existing,
                request.title,
                request.description,
                visibility,
                &candidate_ids,
            )
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;

        Response::ok(PublishCollectionResponse {
            collection_id: outcome.collection_id,
            url: workshop_parse::workshop_url(outcome.collection_id),
            mods: to_proto_items(outcome.mods),
            unresolved: outcome.unresolved,
            created: outcome.created,
            ..Default::default()
        })
    }

    async fn delete_collection<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, DeleteCollectionRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<DeleteCollectionResponse> + Send + use<'a>> {
        let collection_id = required_collection_id(request.collection_id)?;
        self.shared
            .delete_collection_impl(collection_id)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(DeleteCollectionResponse::default())
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
    async fn begin_qr_login<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, BeginQrLoginRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<BeginQrLoginResponse> + Send + use<'a>> {
        let (session, challenge_url) = steam::begin_qr_login()
            .await
            .map_err(|e| ConnectError::internal(format!("failed to begin QR login: {e:#}")))?;

        let session_id = uuid::Uuid::now_v7().to_string();
        self.shared
            .qr_logins
            .lock()
            .unwrap()
            .insert(session_id.clone(), QrLoginState::Pending);

        // The blocking wait happens here, off the RPC path, so PollQrLogin
        // can answer immediately. On confirmation this persists the
        // session and exits exactly as RefreshSteamAuth does -- see that
        // handler's own doc for why a restart beats hot-swapping the
        // connection pool.
        let shared = self.shared.clone();
        let id = session_id.clone();
        tokio::spawn(async move {
            let outcome = steam::poll_qr_login(session).await;
            let (state, confirmed) = match outcome {
                Ok((username, refresh_token)) => {
                    let persisted = secrets::write_session(
                        &shared.client,
                        &shared.namespace,
                        &shared.steam_session_secret_name,
                        &Session {
                            user: username.clone(),
                            refresh_token,
                        },
                    )
                    .await;
                    match persisted {
                        Ok(()) => (QrLoginState::Confirmed { username }, true),
                        Err(e) => (
                            QrLoginState::Failed {
                                error: format!("failed to persist new Steam session: {e:#}"),
                            },
                            false,
                        ),
                    }
                }
                Err(e) => (
                    QrLoginState::Failed {
                        error: format!("{e:#}"),
                    },
                    false,
                ),
            };
            shared.qr_logins.lock().unwrap().insert(id, state);

            if confirmed {
                info!("new Steam session established via QR, restarting to pick it up");
                // Longer than RefreshSteamAuth's one second: the caller
                // only learns this succeeded by polling, so the process
                // has to stay up long enough to answer at least one more
                // poll before it goes away.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                std::process::exit(0);
            }
        });

        Response::ok(BeginQrLoginResponse {
            session_id,
            challenge_url,
            ..Default::default()
        })
    }

    async fn poll_qr_login<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, PollQrLoginRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<PollQrLoginResponse> + Send + use<'a>> {
        let state = self.shared.qr_logins.lock().unwrap();
        match state.get(request.session_id) {
            Some(QrLoginState::Pending) => Response::ok(PollQrLoginResponse {
                confirmed: false,
                ..Default::default()
            }),
            Some(QrLoginState::Confirmed { username }) => Response::ok(PollQrLoginResponse {
                confirmed: true,
                username: username.clone(),
                ..Default::default()
            }),
            Some(QrLoginState::Failed { error }) => Err(ConnectError::internal(error.clone())),
            // Also what a caller polling after this process restarted
            // sees -- which, on the happy path, means the login worked.
            None => Err(ConnectError::not_found(
                "unknown QR login session -- it may have completed and restarted this service, \
                 or expired",
            )),
        }
    }

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

/// A proto `uint64` collection id, where 0 is proto3's "absent" rather
/// than an id anything could have.
fn required_collection_id(id: u64) -> Result<u64, ConnectError> {
    if id == 0 {
        return Err(ConnectError::invalid_argument("collection_id is required"));
    }
    Ok(id)
}

/// Steam's visibility as the proto enum. A value this build doesn't know
/// goes out as UNSPECIFIED rather than as whichever known one is closest:
/// the reader is a person deciding whether to change it.
fn proto_visibility(
    visibility: Option<Visibility>,
) -> buffa::EnumValue<protocol::proto::sync::v1::CollectionVisibility> {
    visibility.map(Visibility::as_proto).unwrap_or(0).into()
}

/// Steam's `time_updated` is unix seconds; the protos carry milliseconds
/// like every other timestamp in them.
fn unix_ms(seconds: u32) -> i64 {
    i64::from(seconds) * 1000
}

fn to_proto_items(mods: Vec<ResolvedMod>) -> Vec<WorkshopItem> {
    mods.into_iter()
        .map(|m| WorkshopItem {
            id: m.mod_id,
            title: m.title,
            file_size: m.file_size,
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use buffa::enumeration::Enumeration;
    use protocol::proto::sync::v1::CollectionVisibility;

    use super::*;

    const CLUSTER: u64 = 76561198000000001;

    fn details(creator: u64) -> collection::CollectionDetails {
        collection::CollectionDetails {
            id: 42,
            title: "Sahatra".into(),
            description: String::new(),
            visibility: Some(Visibility::Unlisted),
            creator,
            time_updated: 0,
            children: vec![],
        }
    }

    #[test]
    fn owned_collections_pass_the_ownership_check() {
        assert!(ensure_owned(&details(CLUSTER), CLUSTER).is_ok());
    }

    #[test]
    fn someone_elses_collection_is_refused_with_a_way_forward() {
        let err = ensure_owned(&details(CLUSTER + 1), CLUSTER).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("another Steam account"), "{msg}");
        assert!(
            msg.contains("copied"),
            "should say what *can* be done: {msg}"
        );
    }

    #[test]
    fn zero_collection_id_is_refused() {
        assert!(required_collection_id(0).is_err());
        assert_eq!(required_collection_id(7).unwrap(), 7);
    }

    #[test]
    fn visibility_goes_out_in_proto_numbering() {
        assert_eq!(
            proto_visibility(Some(Visibility::Public)).to_i32(),
            CollectionVisibility::COLLECTION_VISIBILITY_PUBLIC.to_i32()
        );
        assert_eq!(
            proto_visibility(Some(Visibility::Unlisted)).to_i32(),
            CollectionVisibility::COLLECTION_VISIBILITY_UNLISTED.to_i32()
        );
        // Unknown to this build: reported as unset, never as some guess.
        assert_eq!(proto_visibility(None).to_i32(), 0);
    }

    #[test]
    fn steam_seconds_become_milliseconds() {
        assert_eq!(unix_ms(1_790_000_000), 1_790_000_000_000);
        assert_eq!(unix_ms(0), 0);
    }

    #[test]
    fn items_keep_order_and_fields() {
        let items = to_proto_items(vec![
            ResolvedMod {
                mod_id: 2,
                title: "b".into(),
                file_size: 20,
            },
            ResolvedMod {
                mod_id: 1,
                title: "a".into(),
                file_size: 10,
            },
        ]);
        assert_eq!(
            items
                .iter()
                .map(|i| (i.id, i.title.as_str(), i.file_size))
                .collect::<Vec<_>>(),
            vec![(2, "b", 20), (1, "a", 10)]
        );
    }
}
