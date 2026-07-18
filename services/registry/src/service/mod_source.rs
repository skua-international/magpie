//! `ModSourceService`: registers/lists/deletes mod sources, independent of
//! any server's lifecycle. Steam-based sources (mod/collection/preset) get
//! resolved through sync-daemon's authenticated session; local (zip)
//! sources bypass Steam and sync-daemon entirely -- see `storage.rs`'s doc
//! comment for why.

use std::path::PathBuf;
use std::sync::Arc;

use std::collections::HashMap;

use buffa::enumeration::EnumValue;
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use protocol::proto::registry::v1::add_mod_source_request::SourceView;
use protocol::proto::registry::v1::{
    AddModSourceRequest, AddModSourceResponse, DeleteModSourceRequest, DeleteModSourceResponse, GetSyncedModRequest,
    GetSyncedModResponse, InvalidateModRequest, InvalidateModResponse, ListModSourcesRequest, ListModSourcesResponse,
    ListSyncedModsRequest, ListSyncedModsResponse, ModSourceInfo, ModSourceKind as ProtoKind, SyncModSourceRequest,
    SyncModSourceResponse, SyncedMod as ProtoSyncedMod,
};
use registry_db::{self as db, ModSourceKind, ModSourceRow};
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
        // One fetch covers every Steam-backed source below, instead of a
        // separate sync-daemon round trip per source.
        let sizes_by_mod_id: HashMap<u64, u64> = self
            .sync_client
            .list_synced_mods()
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?
            .into_iter()
            .map(|m| (m.mod_id, m.size_bytes))
            .collect();

        let mut sources = Vec::with_capacity(rows.len());
        for row in rows {
            let size_bytes = self.source_size_bytes(&row, &sizes_by_mod_id).await;
            sources.push(ModSourceInfo {
                id: row.id.to_string(),
                kind: EnumValue::Known(kind_to_proto(row.kind)),
                reference: row.reference,
                display_name: row.display_name,
                created_at_unix_ms: row.created_at.timestamp_millis(),
                size_bytes,
                ..Default::default()
            });
        }
        Response::ok(ListModSourcesResponse { sources, ..Default::default() })
    }

    async fn sync_mod_source<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SyncModSourceRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<SyncModSourceResponse> + Send + use<'a>> {
        let id = Uuid::parse_str(request.id).map_err(|_| ConnectError::invalid_argument("id is not a valid UUID"))?;
        let Some(row) = db::get_mod_source(&self.pool, id).await.map_err(|e| ConnectError::internal(format!("{e:#}")))? else {
            return Err(ConnectError::not_found(format!("no such mod source: {}", request.id)));
        };
        if row.kind == ModSourceKind::Local {
            return Err(ConnectError::invalid_argument("local mod sources have no Steam content to sync"));
        }

        self.sync_client.refresh_source(&id.to_string()).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        let job_id = self.sync_client.claim().await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(SyncModSourceResponse { job_id, ..Default::default() })
    }

    async fn list_synced_mods<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListSyncedModsRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListSyncedModsResponse> + Send + use<'a>> {
        let mods = self
            .sync_client
            .list_synced_mods()
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?
            .into_iter()
            .map(|m| ProtoSyncedMod {
                mod_id: m.mod_id,
                manifest_id: m.manifest_id,
                size_bytes: m.size_bytes,
                title: m.title,
                ..Default::default()
            })
            .collect();
        Response::ok(ListSyncedModsResponse { mods, ..Default::default() })
    }

    async fn get_synced_mod<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetSyncedModRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetSyncedModResponse> + Send + use<'a>> {
        let (m, source_ids) =
            self.sync_client.get_synced_mod(request.mod_id).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        let m = m.map(|m| ProtoSyncedMod {
            mod_id: m.mod_id,
            manifest_id: m.manifest_id,
            size_bytes: m.size_bytes,
            title: m.title,
            ..Default::default()
        });

        let mut mod_sources = Vec::with_capacity(source_ids.len());
        for source_id in &source_ids {
            let Ok(id) = Uuid::parse_str(source_id) else { continue };
            if let Ok(Some(row)) = db::get_mod_source(&self.pool, id).await {
                mod_sources.push(ModSourceInfo {
                    id: row.id.to_string(),
                    kind: EnumValue::Known(kind_to_proto(row.kind)),
                    reference: row.reference,
                    display_name: row.display_name,
                    created_at_unix_ms: row.created_at.timestamp_millis(),
                    ..Default::default()
                });
            }
        }
        Response::ok(GetSyncedModResponse { r#mod: m.into(), mod_sources, ..Default::default() })
    }

    async fn invalidate_mod<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, InvalidateModRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<InvalidateModResponse> + Send + use<'a>> {
        self.sync_client.invalidate_mod(request.mod_id).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(InvalidateModResponse::default())
    }
}

impl ModSourceServiceImpl {
    async fn register_preset(&self, source_id: &str, candidate_ids: &[u64]) -> Result<(), ConnectError> {
        self.sync_client.register_source(source_id, candidate_ids).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Ok(())
    }

    /// A source's total on-disk size: local kind reads its own extracted
    /// directory directly; Steam-backed kinds sum whatever mods it
    /// currently resolves to against the already-fetched size map (a mod
    /// shared by multiple sources counts fully toward each -- see
    /// `ModSourceInfo.size_bytes`'s own doc for why that's correct here,
    /// unlike `GetDiskUsage`'s deduplicated cluster-wide total).
    async fn source_size_bytes(&self, row: &ModSourceRow, sizes_by_mod_id: &HashMap<u64, u64>) -> u64 {
        if row.kind == ModSourceKind::Local {
            return storage::local_mod_size(&self.local_content_root, &row.reference);
        }
        match self.sync_client.get_source_mods(&row.id.to_string()).await {
            Ok(mod_ids) => mod_ids.iter().filter_map(|id| sizes_by_mod_id.get(id)).sum(),
            Err(e) => {
                tracing::warn!("failed to get source mods for {} while computing size: {e:#}", row.id);
                0
            }
        }
    }
}
