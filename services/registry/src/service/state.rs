//! `AdminService::ExportState`/`ImportState` -- see the proto's own doc
//! for scope (mod source registrations, ConfigMaps, ArmaServer specs;
//! deliberately not Postgres data, synced file content, ACL grants, or
//! any live credential).
//!
//! Creates `ArmaServer` objects directly against the Kubernetes API
//! (`Api<ArmaServer>::create`) rather than calling services/server-api's
//! own `CreateServer` RPC -- that RPC can't set `cdlc`/`profiling`/
//! `params` at all (see proto/controller/v1/controller.proto's own
//! comment on why), which would silently drop them on every import. This
//! is also why registry's RBAC gained direct `armaservers`/`configmaps`
//! access for this feature alone (see charts/magpie/templates/
//! registry-rbac.yaml) -- everything else in this service works through
//! the ModSource CRD it already owned.

use std::collections::HashMap;

use buffa::enumeration::EnumValue;
use connectrpc::ConnectError;
use crd::{ArmaServer, ArmaServerSpec, DesiredState, ModSource, ModSourceInput};
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::ResourceExt;
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use protocol::proto::registry::v1::{
    ExportStateResponse, ExportedConfigMap, ExportedDesiredState, ExportedModSource,
    ExportedServer, ImportStateRequest, ImportStateResponse, ModSourceKind as ProtoKind,
};
use uuid::Uuid;

use super::admin::AdminServiceImpl;
use super::mod_source::to_mod_source_info;

/// The join key used for both `id_to_reference` (export) and
/// `reference_to_new_id` (import) -- almost always just `reference`
/// itself, except every PRESET source registered from inline HTML
/// content shares the literal, non-unique string "(uploaded HTML)" (see
/// `reference_for` in mod_source.rs), which would collide across
/// distinct sources. Folding in `resolved_mod_ids` (sorted/deduped, so
/// the same mod set always produces the same key) disambiguates those
/// without needing the original HTML markup at all -- a preset's real
/// identity is the mod set it resolves to, not its wrapper HTML.
fn match_key(kind: ProtoKind, reference: &str, resolved_mod_ids: &[u64]) -> String {
    if kind == ProtoKind::Preset && reference == "(uploaded HTML)" {
        let mut ids = resolved_mod_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let ids = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        format!("(uploaded HTML: {ids})")
    } else {
        reference.to_string()
    }
}

/// Synthesizes minimal HTML content containing one `filedetails/?id=`
/// link per mod ID -- workshop_parse::parse_preset_html only ever scans
/// raw text for that pattern (see its own doc), so this parses back to
/// exactly `mod_ids` regardless of the fact that it isn't the original
/// markup.
fn synthetic_preset_html(mod_ids: &[u64]) -> String {
    mod_ids
        .iter()
        .map(|id| {
            format!("<a href=\"https://steamcommunity.com/sharedfiles/filedetails/?id={id}\"></a>")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl AdminServiceImpl {
    fn mod_source_api(&self) -> Api<ModSource> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn arma_server_api(&self) -> Api<ArmaServer> {
        Api::namespaced(self.client.clone(), &self.armaserver_namespace)
    }

    fn configmap_api(&self) -> Api<ConfigMap> {
        Api::namespaced(self.client.clone(), &self.armaserver_namespace)
    }

    pub(crate) async fn export_state_impl(&self) -> Result<ExportStateResponse, ConnectError> {
        let mod_sources = self
            .mod_source_api()
            .list(&ListParams::default())
            .await
            .map_err(|e| ConnectError::internal(format!("failed to list mod sources: {e:#}")))?;

        // id (the ModSource object's own name) -> its match_key, so
        // servers below can be exported by that instead of an ID that
        // means nothing on a different cluster. Not the same as
        // ExportedModSource.reference itself -- see match_key's own doc
        // for why those need to differ for PRESET-from-HTML-content.
        let mut id_to_key = HashMap::new();
        let mut exported_mod_sources = Vec::new();
        for obj in &mod_sources.items {
            let info = to_mod_source_info(obj);
            let kind = info.kind.as_known().unwrap_or(ProtoKind::Unspecified);
            // Only actually needed for the PRESET/"(uploaded HTML)" case
            // (see ExportedModSource.resolved_mod_ids's own doc) -- kept
            // regardless of kind since it costs nothing and status is
            // already in hand.
            let resolved_mod_ids = obj
                .status
                .as_ref()
                .map(|s| s.resolved_mod_ids.clone())
                .unwrap_or_default();
            id_to_key.insert(
                info.id.clone(),
                match_key(kind, &info.reference, &resolved_mod_ids),
            );
            exported_mod_sources.push(ExportedModSource {
                kind: info.kind,
                reference: info.reference,
                display_name: info.display_name,
                resolved_mod_ids,
                ..Default::default()
            });
        }

        let mut warnings = Vec::new();

        let servers = self
            .arma_server_api()
            .list(&ListParams::default())
            .await
            .map_err(|e| ConnectError::internal(format!("failed to list servers: {e:#}")))?;

        let mut config_map_names: Vec<String> = vec![self.arma_config_baseline.clone()];
        let mut exported_servers = Vec::new();
        for obj in &servers.items {
            let name = obj.name_any();
            let mut mod_source_references = Vec::new();
            for id in &obj.spec.mod_source_ids {
                match id_to_key.get(id) {
                    Some(key) => mod_source_references.push(key.clone()),
                    None => warnings.push(format!(
                        "server {name}: mod source {id} no longer exists, dropped from export"
                    )),
                }
            }
            if let Some(cm) = &obj.spec.config_map {
                config_map_names.push(cm.clone());
            }
            exported_servers.push(ExportedServer {
                name,
                port: obj.spec.port as u32,
                mod_source_references,
                config_map: obj.spec.config_map.clone(),
                cdlc: obj.spec.cdlc.clone(),
                profiling: obj.spec.profiling,
                params: obj.spec.params.clone(),
                desired_state: EnumValue::Known(match obj.spec.desired_state {
                    DesiredState::Running => ExportedDesiredState::Running,
                    DesiredState::Stopped => ExportedDesiredState::Stopped,
                }),
                ..Default::default()
            });
        }

        config_map_names.sort();
        config_map_names.dedup();
        let mut exported_config_maps = Vec::new();
        let cm_api = self.configmap_api();
        for name in config_map_names {
            match cm_api.get(&name).await {
                Ok(cm) => exported_config_maps.push(ExportedConfigMap {
                    name,
                    data: cm.data.unwrap_or_default().into_iter().collect(),
                    ..Default::default()
                }),
                Err(kube::Error::Api(e)) if e.code == 404 => {
                    warnings.push(format!("ConfigMap {name} not found, skipped from export"));
                }
                Err(e) => {
                    return Err(ConnectError::internal(format!(
                        "failed to read ConfigMap {name}: {e:#}"
                    )));
                }
            }
        }

        Ok(ExportStateResponse {
            exported_at_rfc3339: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            mod_sources: exported_mod_sources,
            config_maps: exported_config_maps,
            servers: exported_servers,
            warnings,
            ..Default::default()
        })
    }

    pub(crate) async fn import_state_impl(
        &self,
        req: ImportStateRequest,
    ) -> Result<ImportStateResponse, ConnectError> {
        let mut warnings = Vec::new();

        // match_key -> the freshly-created ModSource's own new ID, so
        // servers below can remap mod_source_references onto it. Only
        // LOCAL (a unique_id, whose actual zip content was never part of
        // this export) and a PRESET that was never resolved before
        // export (empty resolved_mod_ids, nothing to reconstruct from)
        // never reach this map -- everything else, including inline-
        // HTML-content presets, is reconstructible. See match_key's own
        // doc for why the map key isn't just `source.reference` as-is.
        let mut reference_to_new_id = HashMap::new();
        for source in &req.mod_sources {
            let kind = source.kind.as_known().unwrap_or(ProtoKind::Unspecified);
            let input = match kind {
                ProtoKind::Mod | ProtoKind::Collection => {
                    Some(ModSourceInput::SteamUrl(source.reference.clone()))
                }
                ProtoKind::Preset if source.reference != "(uploaded HTML)" => {
                    Some(ModSourceInput::HtmlUrl(source.reference.clone()))
                }
                ProtoKind::Preset if !source.resolved_mod_ids.is_empty() => Some(
                    ModSourceInput::HtmlContent(synthetic_preset_html(&source.resolved_mod_ids)),
                ),
                _ => None,
            };
            let Some(input) = input else {
                warnings.push(format!(
                    "mod source {:?} ({}): content wasn't part of the export, skipped",
                    kind, source.reference
                ));
                continue;
            };

            let new_id = Uuid::now_v7().to_string();
            let obj = ModSource {
                metadata: ObjectMeta {
                    name: Some(new_id.clone()),
                    ..Default::default()
                },
                spec: crd::ModSourceSpec { source: input },
                status: None,
            };
            match self
                .mod_source_api()
                .create(&PostParams::default(), &obj)
                .await
            {
                Ok(_) => {
                    let key = match_key(kind, &source.reference, &source.resolved_mod_ids);
                    reference_to_new_id.insert(key, new_id);
                }
                Err(e) => warnings.push(format!(
                    "mod source {}: failed to create: {e:#}",
                    source.reference
                )),
            }
        }

        let cm_api = self.configmap_api();
        for cm in &req.config_maps {
            let obj = ConfigMap {
                metadata: ObjectMeta {
                    name: Some(cm.name.clone()),
                    ..Default::default()
                },
                data: Some(
                    cm.data
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                ),
                ..Default::default()
            };
            let result = match cm_api.get(&cm.name).await {
                Ok(_) => {
                    let patch = serde_json::json!({ "data": cm.data });
                    cm_api
                        .patch(&cm.name, &PatchParams::default(), &Patch::Merge(patch))
                        .await
                        .map(|_| ())
                }
                Err(kube::Error::Api(e)) if e.code == 404 => cm_api
                    .create(&PostParams::default(), &obj)
                    .await
                    .map(|_| ()),
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                warnings.push(format!("ConfigMap {}: failed to apply: {e:#}", cm.name));
            }
        }

        let server_api = self.arma_server_api();
        for server in &req.servers {
            let mut mod_source_ids = Vec::new();
            for reference in &server.mod_source_references {
                match reference_to_new_id.get(reference) {
                    Some(id) => mod_source_ids.push(id.clone()),
                    None => warnings.push(format!(
                        "server {}: mod source {reference} wasn't imported, dropped from its mod list",
                        server.name
                    )),
                }
            }
            let desired_state = match server.desired_state.as_known().unwrap_or_default() {
                ExportedDesiredState::Stopped => DesiredState::Stopped,
                _ => DesiredState::Running,
            };
            let spec = ArmaServerSpec {
                mod_source_ids,
                port: server.port as u16,
                cdlc: server.cdlc.clone(),
                profiling: server.profiling,
                params: server.params.clone(),
                desired_state,
                config_map: server.config_map.clone(),
            };
            let obj = ArmaServer {
                metadata: ObjectMeta {
                    name: Some(server.name.clone()),
                    ..Default::default()
                },
                spec,
                status: None,
            };
            match server_api.create(&PostParams::default(), &obj).await {
                Ok(_) => {}
                Err(kube::Error::Api(e)) if e.code == 409 => {
                    warnings.push(format!(
                        "server {}: already exists, skipped (not overwritten)",
                        server.name
                    ));
                }
                Err(e) => warnings.push(format!("server {}: failed to create: {e:#}", server.name)),
            }
        }

        Ok(ImportStateResponse {
            warnings,
            ..Default::default()
        })
    }
}
