//! `ModSourceService`: registers/lists/deletes mod sources, independent of
//! any server's lifecycle. Steam-based sources (mod/collection/preset) get
//! resolved through sync-daemon's authenticated session; local (zip)
//! sources bypass Steam and sync-daemon entirely -- see `storage.rs`'s doc
//! comment for why.

use std::path::PathBuf;
use std::sync::Arc;

use buffa::enumeration::EnumValue;
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use protocol::proto::registry::v1::add_mod_source_request::SourceView;
use protocol::proto::registry::v1::{
    AddModSourceRequest, AddModSourceResponse, DeleteModSourceRequest, DeleteModSourceResponse, ListModSourcesRequest,
    ListModSourcesResponse, ModSourceInfo, ModSourceKind as ProtoKind,
};
use registry_db::{self as db, ModSourceKind};
use sqlx::PgPool;
use sync_client::SyncClient;
use uuid::Uuid;

use crate::storage;

pub struct ModSourceServiceImpl {
    pool: PgPool,
    sync_client: Arc<SyncClient>,
    local_content_root: PathBuf,
}

impl ModSourceServiceImpl {
    pub fn new(pool: PgPool, sync_client: Arc<SyncClient>, local_content_root: PathBuf) -> Arc<Self> {
        Arc::new(Self { pool, sync_client, local_content_root })
    }
}

fn kind_to_proto(kind: ModSourceKind) -> ProtoKind {
    match kind {
        ModSourceKind::Mod => ProtoKind::Mod,
        ModSourceKind::Collection => ProtoKind::Collection,
        ModSourceKind::Local => ProtoKind::Local,
        ModSourceKind::Preset => ProtoKind::Preset,
    }
}

impl protocol::proto::registry::v1::ModSourceService for ModSourceServiceImpl {
    async fn add_mod_source<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, AddModSourceRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<AddModSourceResponse> + Send + use<'a>> {
        let id = Uuid::now_v7();
        let source_id = id.to_string();

        let (kind, reference, display_name) = match request.source.clone() {
            Some(SourceView::HtmlUrl(url)) => {
                let candidate_ids = workshop_parse::extract_candidate_ids(url)
                    .await
                    .map_err(|e| ConnectError::invalid_argument(format!("failed to read preset from {url}: {e:#}")))?;
                self.register_preset(&source_id, &candidate_ids).await?;
                (ModSourceKind::Preset, url.to_string(), String::new())
            }
            Some(SourceView::HtmlContent(html)) => {
                let candidate_ids = workshop_parse::parse_preset_html(html);
                if candidate_ids.is_empty() {
                    return Err(ConnectError::invalid_argument("no filedetails links found in html_content"));
                }
                self.register_preset(&source_id, &candidate_ids).await?;
                (ModSourceKind::Preset, "(uploaded HTML)".to_string(), String::new())
            }
            Some(SourceView::SteamUrl(url)) => {
                let candidate_id = workshop_parse::extract_single_id(url)
                    .ok_or_else(|| ConnectError::invalid_argument(format!("not a recognizable Workshop filedetails URL: {url}")))?;
                let result = self
                    .sync_client
                    .register_source(&source_id, &[candidate_id])
                    .await
                    .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
                // A candidate that resolved to exactly itself is a plain
                // mod; anything else (0, or >1 expanded members) came from
                // a collection.
                let kind = if result.mod_ids == [candidate_id] { ModSourceKind::Mod } else { ModSourceKind::Collection };
                (kind, url.to_string(), result.root_title)
            }
            Some(SourceView::LocalMod(local)) => {
                storage::extract_local_mod(&self.local_content_root, local.unique_id, local.zip_content)
                    .map_err(|e| ConnectError::invalid_argument(format!("{e:#}")))?;
                (ModSourceKind::Local, local.unique_id.to_string(), String::new())
            }
            None => return Err(ConnectError::invalid_argument("missing source")),
        };

        db::insert_mod_source(&self.pool, id, kind, &reference, &display_name)
            .await
            .map_err(|e| ConnectError::internal(format!("failed to persist mod source: {e:#}")))?;

        Response::ok(AddModSourceResponse { id: source_id, ..Default::default() })
    }

    async fn delete_mod_source<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, DeleteModSourceRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<DeleteModSourceResponse> + Send + use<'a>> {
        let id = Uuid::parse_str(request.id).map_err(|_| ConnectError::invalid_argument("id is not a valid UUID"))?;

        let Some(row) = db::get_mod_source(&self.pool, id).await.map_err(|e| ConnectError::internal(format!("{e:#}")))? else {
            return Err(ConnectError::not_found(format!("no such mod source: {}", request.id)));
        };

        match row.kind {
            ModSourceKind::Local => {
                storage::delete_local_mod(&self.local_content_root, &row.reference).map_err(|e| ConnectError::internal(format!("{e:#}")))?;
            }
            ModSourceKind::Mod | ModSourceKind::Collection | ModSourceKind::Preset => {
                self.sync_client.deregister_source(&id.to_string()).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
            }
        }

        db::delete_mod_source(&self.pool, id).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(DeleteModSourceResponse::default())
    }

    async fn list_mod_sources<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListModSourcesRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListModSourcesResponse> + Send + use<'a>> {
        let rows = db::list_mod_sources(&self.pool).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        let sources = rows
            .into_iter()
            .map(|row| ModSourceInfo {
                id: row.id.to_string(),
                kind: EnumValue::Known(kind_to_proto(row.kind)),
                reference: row.reference,
                display_name: row.display_name,
                created_at_unix_ms: row.created_at.timestamp_millis(),
                ..Default::default()
            })
            .collect();
        Response::ok(ListModSourcesResponse { sources, ..Default::default() })
    }
}

impl ModSourceServiceImpl {
    async fn register_preset(&self, source_id: &str, candidate_ids: &[u64]) -> Result<(), ConnectError> {
        self.sync_client.register_source(source_id, candidate_ids).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Ok(())
    }
}
