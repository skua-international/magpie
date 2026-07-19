//! `AdminService`: cluster-wide storage accounting, Steam session
//! refresh, and (see state.rs) declarative cluster state export/import.

use std::sync::Arc;

use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use kube::Client;
use protocol::proto::registry::v1::{
    ExportStateRequest, ExportStateResponse, GetDiskUsageRequest, GetDiskUsageResponse,
    ImportStateRequest, ImportStateResponse, RefreshSteamAuthRequest, RefreshSteamAuthResponse,
};
use sqlx::PgPool;
use sync_client::SyncClient;

pub struct AdminServiceImpl {
    pool: PgPool,
    sync_client: Arc<SyncClient>,
    pub(crate) client: Client,
    /// Namespace for ModSource objects -- see Config::namespace's own doc.
    pub(crate) namespace: String,
    /// Namespace for ArmaServer objects and ConfigMaps -- see
    /// Config::armaserver_namespace's own doc for why this is kept
    /// distinct from `namespace` above.
    pub(crate) armaserver_namespace: String,
    pub(crate) arma_config_baseline: String,
}

impl AdminServiceImpl {
    pub fn new(
        pool: PgPool,
        sync_client: Arc<SyncClient>,
        client: Client,
        namespace: String,
        armaserver_namespace: String,
        arma_config_baseline: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            sync_client,
            client,
            namespace,
            armaserver_namespace,
            arma_config_baseline,
        })
    }
}

impl protocol::proto::registry::v1::AdminService for AdminServiceImpl {
    async fn get_disk_usage<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetDiskUsageRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetDiskUsageResponse> + Send + use<'a>> {
        let stats = self
            .sync_client
            .sync_stats()
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;

        let missions = registry_db::list_missions(&self.pool)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        let missions_bytes: u64 = missions.iter().map(|m| m.filesize as u64).sum();

        // Local mod content isn't currently included -- it's registry's
        // own storage (see storage.rs), not sync-daemon's, and has no
        // equivalent cluster-wide dedup story since local unique_ids can't
        // meaningfully overlap the way a shared Steam mod_id can.
        // ModSourceInfo.size_bytes still reports each local source's own
        // size individually.
        let mods_bytes = stats.mods_bytes;
        let game_files_bytes = stats.game_files_bytes;
        let total_bytes = mods_bytes + missions_bytes + game_files_bytes;

        Response::ok(GetDiskUsageResponse {
            mods_bytes,
            missions_bytes,
            game_files_bytes,
            total_bytes,
            ..Default::default()
        })
    }

    async fn refresh_steam_auth<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, RefreshSteamAuthRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<RefreshSteamAuthResponse> + Send + use<'a>> {
        self.sync_client
            .refresh_steam_auth(&request.username, &request.refresh_token)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(RefreshSteamAuthResponse::default())
    }

    async fn export_state<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ExportStateRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ExportStateResponse> + Send + use<'a>> {
        let resp = self.export_state_impl().await?;
        Response::ok(resp)
    }

    async fn import_state<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, ImportStateRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ImportStateResponse> + Send + use<'a>> {
        // The handler below builds/holds owned ExportedModSource/
        // ExportedConfigMap/ExportedServer values across several awaits
        // (K8s API calls) -- easier as one owned ImportStateRequest than
        // threading the zero-copy view's lifetime through all of that.
        let owned = request.to_owned_message();
        let resp = self.import_state_impl(owned).await?;
        Response::ok(resp)
    }
}
