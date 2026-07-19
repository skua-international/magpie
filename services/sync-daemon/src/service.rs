use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use protocol::proto::sync::v1::{
    ClaimJobState, ClaimRequest, ClaimResponse, DeregisterSourceRequest, DeregisterSourceResponse,
    GetClaimStatusRequest, GetClaimStatusResponse, GetSourceModsRequest, GetSourceModsResponse,
    GetSyncStatsRequest, GetSyncStatsResponse, GetSyncedModRequest, GetSyncedModResponse,
    InvalidateModRequest, InvalidateModResponse, ListSyncedModsRequest, ListSyncedModsResponse,
    RefreshSourceRequest, RefreshSourceResponse, RefreshSteamAuthRequest, RefreshSteamAuthResponse,
    RegisterSourceRequest, RegisterSourceResponse, ResolvedMod as ProtoResolvedMod, SyncService,
    SyncedMod,
};
use steam_sync::cache::SyncState;
use steam_sync::steam::{self, CmPool, ResolvedMod, SyncTasks};
use steam_sync::workshop;
use tokio::sync::Semaphore;
use tracing::{error, info};
use uuid::Uuid;

use crate::secrets::{self, Session};

/// A `Claim` job's current state -- `Claim` starts one and returns its ID
/// immediately rather than blocking for the (potentially minutes-long,
/// cold-cache) duration of the whole resolve+sync+claim; `GetClaimStatus`
/// polls this.
#[derive(Clone)]
enum JobStatus {
    Running,
    Done { claim_path: String },
    Failed { error: String },
}

/// In-memory job table. Deliberately simple -- an unbounded map that never
/// evicts finished jobs is fine for a first pass (the daemon's own
/// lifetime between restarts bounds it in practice), but worth revisiting
/// (TTL / explicit ack-and-remove) before this sees real traffic volume.
type Jobs = Mutex<HashMap<String, JobStatus>>;

/// Shared state, `Arc`-wrapped so it can be cloned cheaply into a spawned
/// `Claim` job -- kept separate from [`SyncServiceImpl`] (which only
/// borrows `&self` per the trait's method signatures) specifically so
/// spawning doesn't need a self-referential `Arc<Self>`.
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
    pub claims_root: PathBuf,
    pub client: kube::Client,
    pub namespace: String,
    pub steam_session_secret_name: String,
    jobs: Jobs,
}

impl Shared {
    pub fn new(
        pool: Option<Arc<CmPool>>,
        sync_state: Arc<SyncState>,
        content_root: PathBuf,
        claims_root: PathBuf,
        client: kube::Client,
        namespace: String,
        steam_session_secret_name: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            sync_state,
            content_root,
            claims_root,
            client,
            namespace,
            steam_session_secret_name,
            jobs: Mutex::new(HashMap::new()),
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
        self.sync_state.upsert_source(source_id, candidate_ids)?;
        self.sync_state.set_source_mods(source_id, &mod_ids)?;
        for m in &outcome.mods {
            self.sync_state.record_mod_title(m.mod_id, &m.title);
        }

        let root_title = match candidate_ids {
            [single] => outcome
                .candidate_titles
                .get(single)
                .cloned()
                .unwrap_or_default(),
            _ => String::new(),
        };

        Ok(RegisterSourceOutcome {
            mods: outcome.mods,
            root_title,
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
            .candidate_ids_for_source(source_id)?
            .ok_or_else(|| anyhow::anyhow!("unknown source: {source_id}"))?;
        self.register_source_impl(&candidate_ids, source_id).await
    }

    async fn run_claim_job(self: Arc<Self>, job_id: String) {
        let result = self.do_claim().await;
        let status = match result {
            Ok(claim_path) => JobStatus::Done { claim_path },
            Err(e) => {
                error!("claim job {job_id} failed: {e:#}");
                JobStatus::Failed {
                    error: format!("{e:#}"),
                }
            }
        };
        self.jobs.lock().unwrap().insert(job_id, status);
    }

    async fn do_claim(&self) -> anyhow::Result<String> {
        let sem = Arc::new(Semaphore::new(steam::SYNC_CONCURRENCY));
        let tasks: Mutex<SyncTasks> = Mutex::new(SyncTasks::new());

        let mut conn = self.pool()?.acquire().await;
        // Server/CDLC depots aren't part of the source registry -- they're
        // always wanted, same as before this rework.
        let result = steam::resolve_and_spawn_server(
            &mut conn,
            &self.content_root,
            false,
            &[],
            sem.clone(),
            &tasks,
            self.sync_state.clone(),
        )
        .await;
        if let Err(e) = &result {
            if steam::is_transient(e) {
                conn.mark_bad();
            }
        }
        drop(conn);
        result?;

        let desired = self.sync_state.desired_mod_ids()?;
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

        let claim_id = Uuid::now_v7();
        let claim_dir = self.claims_root.join(claim_id.to_string());
        let claim_path = steam_sync::claim::claim(&self.content_root, &claim_dir)?;
        Ok(claim_path.display().to_string())
    }
}

pub struct RegisterSourceOutcome {
    pub mods: Vec<ResolvedMod>,
    pub root_title: String,
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
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(DeregisterSourceResponse::default())
    }

    async fn claim<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ClaimRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ClaimResponse> + Send + use<'a>> {
        let job_id = Uuid::now_v7().to_string();
        self.shared
            .jobs
            .lock()
            .unwrap()
            .insert(job_id.clone(), JobStatus::Running);

        let shared = self.shared.clone();
        let spawned_job_id = job_id.clone();
        tokio::spawn(async move { shared.run_claim_job(spawned_job_id).await });

        info!("started claim job {job_id}");
        Response::ok(ClaimResponse {
            job_id,
            ..Default::default()
        })
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

    async fn get_claim_status<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetClaimStatusRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetClaimStatusResponse> + Send + use<'a>> {
        let jobs = self.shared.jobs.lock().unwrap();
        let status = jobs
            .get(request.job_id)
            .ok_or_else(|| ConnectError::not_found(format!("unknown job {}", request.job_id)))?
            .clone();
        drop(jobs);

        use buffa::enumeration::EnumValue;
        let response = match status {
            JobStatus::Running => GetClaimStatusResponse {
                state: EnumValue::Known(ClaimJobState::Running),
                ..Default::default()
            },
            JobStatus::Done { claim_path } => GetClaimStatusResponse {
                state: EnumValue::Known(ClaimJobState::Done),
                claim_path,
                ..Default::default()
            },
            JobStatus::Failed { error } => GetClaimStatusResponse {
                state: EnumValue::Known(ClaimJobState::Failed),
                error,
                ..Default::default()
            },
        };
        Response::ok(response)
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
            .map(|m| SyncedMod {
                mod_id: m.mod_id,
                manifest_id: m.manifest_id,
                size_bytes: m.size_bytes,
                title: m.title,
                ..Default::default()
            });
        let source_ids = self.shared.sync_state.sources_for_mod(mod_id);
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
            mods_bytes: self.shared.sync_state.total_mods_size(),
            game_files_bytes: self.shared.sync_state.total_game_files_size(),
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
        self.shared.sync_state.invalidate(&key);
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
