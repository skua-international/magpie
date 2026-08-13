//! `MissionService`: uploaded missions live on the same shared local-content
//! volume as local mods (see `storage.rs`), mounted read-only into every
//! server Pod -- missions aren't server-scoped, uploading one makes it
//! available everywhere.

use std::path::PathBuf;
use std::sync::Arc;

use authn::authz::AuthIdentity;
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use protocol::proto::registry::v1::{
    DeleteMissionRequest, DeleteMissionResponse, GetMissionRequest, ListMissionsRequest,
    ListMissionsResponse, MissionInfo, SetMissionMetadataRequest, UploadMissionRequest,
};
use registry_db as db;
use sqlx::PgPool;
use uuid::Uuid;

use crate::storage;

pub struct MissionServiceImpl {
    pool: PgPool,
    local_content_root: PathBuf,
}

impl MissionServiceImpl {
    pub fn new(pool: PgPool, local_content_root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            pool,
            local_content_root,
        })
    }
}

fn to_info(row: db::MissionRow) -> MissionInfo {
    MissionInfo {
        id: row.id.to_string(),
        name: row.name,
        filesize: row.filesize as u64,
        created_at_unix_ms: row.created_at.timestamp_millis(),
        created_by: row.created_by,
        metadata: db::metadata_map(&row.metadata).into_iter().collect(),
        ..Default::default()
    }
}

/// Builds the jsonb value stored for a mission's metadata.
///
/// Keys are validated the same way the annotation-backed metadata on
/// servers and mod sources is, even though Postgres would accept
/// anything -- so the same key is legal wherever an operator puts it,
/// rather than a mission accepting one that a mod source would reject.
fn metadata_json<'a>(entries: impl Iterator<Item = (&'a str, &'a str)>) -> serde_json::Value {
    serde_json::Value::Object(
        entries
            .filter(|(k, _)| !k.is_empty())
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect(),
    )
}

impl protocol::proto::registry::v1::MissionService for MissionServiceImpl {
    async fn set_mission_metadata<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SetMissionMetadataRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<MissionInfo> + Send + use<'a>> {
        let id = uuid::Uuid::parse_str(request.id)
            .map_err(|_| ConnectError::invalid_argument("id is not a valid UUID"))?;

        let updated = db::set_mission_metadata(
            &self.pool,
            id,
            &metadata_json(request.metadata.iter().map(|(k, v)| (*k, *v))),
        )
        .await
        .map_err(|e| ConnectError::internal(format!("failed to update metadata: {e:#}")))?;
        if !updated {
            return Err(ConnectError::not_found(format!("no such mission: {id}")));
        }

        let row = db::get_mission(&self.pool, id)
            .await
            .map_err(|e| ConnectError::internal(format!("failed to re-read mission: {e:#}")))?
            .ok_or_else(|| ConnectError::not_found(format!("no such mission: {id}")))?;
        Response::ok(to_info(row))
    }

    async fn upload_mission<'a>(
        &'a self,
        ctx: RequestContext,
        request: ServiceRequest<'_, UploadMissionRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<MissionInfo> + Send + use<'a>> {
        let subject = ctx
            .extensions()
            .get::<AuthIdentity>()
            .map(|identity| identity.subject.clone())
            .ok_or_else(|| ConnectError::internal("missing authenticated identity"))?;

        let id = match request.id {
            Some(existing) => Uuid::parse_str(existing)
                .map_err(|_| ConnectError::invalid_argument("id is not a valid UUID"))?,
            None => Uuid::now_v7(),
        };

        if request.name.is_empty() {
            return Err(ConnectError::invalid_argument("name is required"));
        }

        storage::write_mission(
            &self.local_content_root,
            id,
            request.name,
            request.pbo_content,
        )
        .map_err(|e| ConnectError::internal(format!("{e:#}")))?;

        db::upsert_mission(
            &self.pool,
            id,
            request.name,
            request.pbo_content.len() as i64,
            &subject,
            &metadata_json(request.metadata.iter().map(|(k, v)| (*k, *v))),
        )
        .await
        .map_err(|e| ConnectError::internal(format!("failed to persist mission: {e:#}")))?;

        let row = db::get_mission(&self.pool, id)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?
            .ok_or_else(|| {
                ConnectError::internal("mission vanished immediately after being written")
            })?;

        Response::ok(to_info(row))
    }

    async fn get_mission<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetMissionRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<MissionInfo> + Send + use<'a>> {
        let id = Uuid::parse_str(request.id)
            .map_err(|_| ConnectError::invalid_argument("id is not a valid UUID"))?;
        let row = db::get_mission(&self.pool, id)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?
            .ok_or_else(|| ConnectError::not_found(format!("no such mission: {}", request.id)))?;
        Response::ok(to_info(row))
    }

    async fn list_missions<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListMissionsRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListMissionsResponse> + Send + use<'a>> {
        let rows = db::list_missions(&self.pool)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(ListMissionsResponse {
            missions: rows.into_iter().map(to_info).collect(),
            ..Default::default()
        })
    }

    async fn delete_mission<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, DeleteMissionRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<DeleteMissionResponse> + Send + use<'a>> {
        let id = Uuid::parse_str(request.id)
            .map_err(|_| ConnectError::invalid_argument("id is not a valid UUID"))?;
        let deleted = db::delete_mission(&self.pool, id)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        if !deleted {
            return Err(ConnectError::not_found(format!(
                "no such mission: {}",
                request.id
            )));
        }
        storage::delete_mission_file(&self.local_content_root, id)
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(DeleteMissionResponse::default())
    }
}
