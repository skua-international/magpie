//! `ModSourceService`: registers/lists/deletes mod sources, independent of
//! any server's lifecycle. Backed by the `ModSource` CRD (see
//! `crates/crd`), not Postgres -- Steam-based sources (mod/collection/
//! preset) get resolved asynchronously by `services/sync-daemon`'s own
//! reconciler; local (zip) sources bypass Steam and sync-daemon entirely
//! and are fully resolved synchronously, right here, at creation time (see
//! `storage.rs`'s doc comment for why local content lives outside the
//! reflink-claim/manifest-tracking machinery).

use std::path::PathBuf;
use std::sync::Arc;

use buffa::enumeration::EnumValue;
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use crd::{ModSource, ModSourceInput, ModSourcePhase, ModSourceStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, ResourceExt};
use protocol::proto::registry::v1::add_mod_source_request::SourceView;
use protocol::proto::registry::v1::{
    AddModSourceRequest, AddModSourceResponse, DeleteModSourceRequest, DeleteModSourceResponse, GetSyncedModRequest,
    GetSyncedModResponse, InvalidateModRequest, InvalidateModResponse, ListModSourcesRequest, ListModSourcesResponse,
    ListSyncedModsRequest, ListSyncedModsResponse, ModSourceInfo, ModSourceKind as ProtoKind, SyncModSourceRequest,
    SyncModSourceResponse, SyncedMod as ProtoSyncedMod,
};
use sync_client::SyncClient;
use uuid::Uuid;

use crate::storage;

pub struct ModSourceServiceImpl {
    client: Client,
    namespace: String,
    sync_client: Arc<SyncClient>,
    local_content_root: PathBuf,
}

impl ModSourceServiceImpl {
    pub fn new(client: Client, namespace: String, sync_client: Arc<SyncClient>, local_content_root: PathBuf) -> Arc<Self> {
        Arc::new(Self { client, namespace, sync_client, local_content_root })
    }

    fn api(&self) -> Api<ModSource> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }
}

/// The human-meaningful reference a `ModSource` was created with -- a
/// Steam URL, a marker for uploaded HTML, or a local mod's unique_id.
/// Always derivable from `spec` alone, never from `status` (which may not
/// have resolved yet).
fn reference_for(spec: &ModSourceInput) -> String {
    match spec {
        ModSourceInput::SteamUrl(url) | ModSourceInput::HtmlUrl(url) => url.clone(),
        ModSourceInput::HtmlContent(_) => "(uploaded HTML)".to_string(),
        ModSourceInput::Local { unique_id } => unique_id.clone(),
    }
}

fn kind_str_to_proto(kind: &str) -> ProtoKind {
    match kind {
        "mod" => ProtoKind::Mod,
        "collection" => ProtoKind::Collection,
        "local" => ProtoKind::Local,
        "preset" => ProtoKind::Preset,
        _ => ProtoKind::Unspecified,
    }
}

fn to_mod_source_info(obj: &ModSource) -> ModSourceInfo {
    let status = obj.status.clone().unwrap_or_default();
    let created_at_unix_ms = obj.creation_timestamp().map(|t| t.0.as_millisecond()).unwrap_or_default();
    ModSourceInfo {
        id: obj.name_any(),
        kind: EnumValue::Known(kind_str_to_proto(&status.kind)),
        reference: reference_for(&obj.spec.source),
        display_name: status.display_name,
        created_at_unix_ms,
        size_bytes: status.size_bytes,
        ..Default::default()
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

        let input = match request.source.clone() {
            Some(SourceView::HtmlUrl(url)) => ModSourceInput::HtmlUrl(url.to_string()),
            Some(SourceView::HtmlContent(html)) => {
                if html.is_empty() {
                    return Err(ConnectError::invalid_argument("html_content is empty"));
                }
                ModSourceInput::HtmlContent(html.to_string())
            }
            Some(SourceView::SteamUrl(url)) => {
                if workshop_parse::extract_single_id(url).is_none() {
                    return Err(ConnectError::invalid_argument(format!("not a recognizable Workshop filedetails URL: {url}")));
                }
                ModSourceInput::SteamUrl(url.to_string())
            }
            Some(SourceView::LocalMod(local)) => {
                storage::extract_local_mod(&self.local_content_root, local.unique_id, local.zip_content)
                    .map_err(|e| ConnectError::invalid_argument(format!("{e:#}")))?;
                ModSourceInput::Local { unique_id: local.unique_id.to_string() }
            }
            None => return Err(ConnectError::invalid_argument("missing source")),
        };

        let is_local = matches!(input, ModSourceInput::Local { .. });
        let obj = ModSource {
            metadata: ObjectMeta { name: Some(source_id.clone()), ..Default::default() },
            spec: crd::ModSourceSpec { source: input },
            status: None,
        };
        self.api().create(&PostParams::default(), &obj).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;

        // Local content needs no Steam session to resolve -- there's
        // nothing for sync-daemon's reconciler to do, so mark it Synced
        // right here instead of leaving it Pending forever.
        if is_local {
            let ModSourceInput::Local { unique_id } = &obj.spec.source else { unreachable!() };
            let size_bytes = storage::local_mod_size(&self.local_content_root, unique_id);
            let status = ModSourceStatus {
                phase: ModSourcePhase::Synced,
                kind: "local".to_string(),
                display_name: String::new(),
                resolved_mod_ids: Vec::new(),
                size_bytes,
                message: String::new(),
            };
            let patch = serde_json::json!({ "status": status });
            self.api()
                .patch_status(&source_id, &PatchParams::default(), &Patch::Merge(patch))
                .await
                .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        }

        Response::ok(AddModSourceResponse { id: source_id, ..Default::default() })
    }

    async fn delete_mod_source<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, DeleteModSourceRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<DeleteModSourceResponse> + Send + use<'a>> {
        let obj = self.api().get(request.id).await.map_err(|e| match e {
            kube::Error::Api(e) if e.code == 404 => ConnectError::not_found(format!("no such mod source: {}", request.id)),
            e => ConnectError::internal(format!("{e:#}")),
        })?;

        if let ModSourceInput::Local { unique_id } = &obj.spec.source {
            storage::delete_local_mod(&self.local_content_root, unique_id).map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        }
        // Steam-backed kinds: sync-daemon's own finalizer cleans up its
        // sources/source_mods rows once this delete actually lands.

        self.api().delete(request.id, &DeleteParams::default()).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(DeleteModSourceResponse::default())
    }

    async fn list_mod_sources<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListModSourcesRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListModSourcesResponse> + Send + use<'a>> {
        let objs = self.api().list(&ListParams::default()).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        let sources = objs.items.iter().map(to_mod_source_info).collect();
        Response::ok(ListModSourcesResponse { sources, ..Default::default() })
    }

    async fn sync_mod_source<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SyncModSourceRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<SyncModSourceResponse> + Send + use<'a>> {
        let obj = self.api().get(request.id).await.map_err(|e| match e {
            kube::Error::Api(e) if e.code == 404 => ConnectError::not_found(format!("no such mod source: {}", request.id)),
            e => ConnectError::internal(format!("{e:#}")),
        })?;
        if matches!(obj.spec.source, ModSourceInput::Local { .. }) {
            return Err(ConnectError::invalid_argument("local mod sources have no Steam content to sync"));
        }

        self.sync_client.refresh_source(request.id).await.map_err(|e| ConnectError::internal(format!("{e:#}")))?;
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
            if let Ok(obj) = self.api().get(source_id).await {
                mod_sources.push(to_mod_source_info(&obj));
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
