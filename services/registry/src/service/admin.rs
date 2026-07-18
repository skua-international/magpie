//! `AdminService`: cluster-wide storage accounting. See the proto's own
//! doc comment for why this exists as a separate, single-figure answer
//! rather than making callers sum `ModSourceInfo.size_bytes` themselves
//! (which double-counts any mod referenced by more than one source).

use std::sync::Arc;

use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use protocol::proto::registry::v1::{GetDiskUsageRequest, GetDiskUsageResponse};
use sqlx::PgPool;
use sync_client::SyncClient;

pub struct AdminServiceImpl {
    pool: PgPool,
    sync_client: Arc<SyncClient>,
}

impl AdminServiceImpl {
    pub fn new(pool: PgPool, sync_client: Arc<SyncClient>) -> Arc<Self> {
        Arc::new(Self { pool, sync_client })
    }
}

impl protocol::proto::registry::v1::AdminService for AdminServiceImpl {
    async fn get_disk_usage<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetDiskUsageRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetDiskUsageResponse> + Send + use<'a>> {
        let stats = self.sync_client.sync_stats().await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;

        let missions = registry_db::list_missions(&self.pool).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
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

        Response::ok(GetDiskUsageResponse { mods_bytes, missions_bytes, game_files_bytes, total_bytes, ..Default::default() })
    }
}
