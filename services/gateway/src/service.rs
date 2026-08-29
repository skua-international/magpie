//! `ServerService`: thin, authenticated translation to `ArmaServer` custom
//! resource CRUD, plus the deployment-like lifecycle RPCs
//! (Update/Start/Stop). The actual provisioning work (resolving mod
//! sources, claiming content, creating the launcher Deployment) happens in
//! the reconciler (`reconcile.rs`), which watches these same objects --
//! this service only needs to make desired state durable, kick off the
//! on-demand actions the reconciler can't infer on its own (mod source
//! resync), and read current state back.

use std::sync::Arc;

use buffa::enumeration::EnumValue;
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use crd::{
    ArmaServer, ArmaServerMetrics, ArmaServerPhase, ArmaServerSpec, DesiredState, ModSource,
    ModSourceInput, port_ranges_overlap,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::{Client, ResourceExt};
/// Ceiling on GetServerLogs' tail. High enough to cover a startup
/// sequence, low enough that a single call can't stream a whole pod's
/// history into a browser.
const MAX_LOG_LINES: u32 = 5000;

use protocol::proto::controller::v1::{
    CreateServerRequest, DeleteServerRequest, DeleteServerResponse,
    DesiredState as ProtoDesiredState, GetServerHealthRequest, GetServerHealthResponse,
    GetServerLogsRequest, GetServerLogsResponse, GetServerRequest, ListServersRequest,
    ListServersResponse, ServerInfo, ServerPhase, StartServerRequest, StopServerRequest,
    UpdateServerRequest,
};
use sync_client::SyncClient;

pub struct ServerServiceImpl {
    client: Client,
    namespace: String,
    sync_client: SyncClient,
}

impl ServerServiceImpl {
    pub fn new(client: Client, namespace: String, sync_client: SyncClient) -> Arc<Self> {
        Arc::new(Self {
            client,
            namespace,
            sync_client,
        })
    }

    fn mod_sources(&self) -> Api<ModSource> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn api(&self) -> Api<ArmaServer> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Re-resolves every Steam-backed mod source this server references
    /// (skips local/zip sources -- there's no Steam content to refresh for
    /// those) and kicks the object back to `Pending` so the reconciler
    /// re-claims and rolls onto the result. Shared by `UpdateServer` and
    /// `StartServer`, which both mean "make sure this server is running
    /// the latest content" -- they only differ in whether `desired_state`
    /// also gets flipped to `Running`.
    async fn resync_and_repending(&self, id: &str) -> Result<ArmaServer, ConnectError> {
        let obj = self
            .api()
            .get(id)
            .await
            .map_err(|_| ConnectError::not_found(format!("no such server: {id}")))?;

        for source_id in &obj.spec.mod_source_ids {
            let Ok(source) = self.mod_sources().get(source_id).await else {
                continue;
            };
            if !matches!(source.spec.source, ModSourceInput::Local { .. }) {
                self.sync_client
                    .refresh_source(source_id)
                    .await
                    .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
            }
        }

        let patch = serde_json::json!({ "status": { "phase": "Pending" } });
        self.api()
            .patch_status(id, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|e| ConnectError::internal(format!("failed to reset status: {e:#}")))
    }

    fn pods(&self) -> Api<k8s_openapi::api::core::v1::Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// The Pod currently backing a server.
    ///
    /// Found by the controller's own Deployment labels rather than by
    /// name: a server's pod name carries a ReplicaSet hash and changes
    /// on every restart, so it can't be derived from the server id.
    /// Newest first, so a pod being replaced doesn't get reported over
    /// the one that just took over.
    async fn server_pod(&self, id: &str) -> Result<k8s_openapi::api::core::v1::Pod, ConnectError> {
        // app=arma-server as well as armaserver=<id>: services/controller
        // puts the same armaserver label on this server's HeadlessClient
        // pods too (reconcile.rs's ensure_hc_deployment), so selecting on
        // it alone would happily return an HC's logs as the server's.
        let params = ListParams::default().labels(&format!("app=arma-server,armaserver={id}"));
        let mut pods = self
            .pods()
            .list(&params)
            .await
            .map_err(|e| ConnectError::internal(format!("failed to list pods for {id}: {e:#}")))?
            .items;
        pods.sort_by(|a, b| {
            b.metadata
                .creation_timestamp
                .cmp(&a.metadata.creation_timestamp)
        });
        pods.into_iter()
            .next()
            .ok_or_else(|| ConnectError::not_found(format!("no running pod for server {id}")))
    }

    /// Rejects `port` if its 5-port range (see `crd::port_range`) overlaps
    /// any other server that's running or intends to be -- checked against
    /// `desired_state` rather than `status.phase`, since a server that's
    /// merely `Pending`/`Claiming` toward `Running` still means to occupy
    /// that range imminently. `exclude_name` lets `StartServer` check
    /// without conflicting against its own already-stored port.
    async fn check_port_conflict(
        &self,
        port: u16,
        exclude_name: Option<&str>,
    ) -> Result<(), ConnectError> {
        let list = self
            .api()
            .list(&ListParams::default())
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        for other in &list.items {
            if Some(other.name_any().as_str()) == exclude_name {
                continue;
            }
            if other.spec.desired_state != DesiredState::Running {
                continue;
            }
            if port_ranges_overlap(port, other.spec.port) {
                return Err(ConnectError::invalid_argument(format!(
                    "port {port} (range {}-{}) conflicts with running/starting server '{}' on port {} (range {}-{})",
                    port,
                    port.saturating_add(4),
                    other.name_any(),
                    other.spec.port,
                    other.spec.port,
                    other.spec.port.saturating_add(4),
                )));
            }
        }
        Ok(())
    }
}

/// Kubernetes object names must satisfy DNS-1123 label rules (lowercase
/// alphanumeric and `-` only, must start/end alphanumeric, max 63
/// chars) -- `name` becomes the `ArmaServer`'s own `metadata.name`
/// verbatim (`create_server`, below), completely unvalidated before
/// this. Confirmed live: a name with a space ("test server") sailed
/// past the old empty-check, then failed deep inside kube-rs's own HTTP
/// client with "Failed to build request: ... invalid uri character" --
/// a raw space breaks `http::Uri`'s strict parser when kube-rs
/// interpolates the name into a request path, and that error gives the
/// caller zero indication *why*. Catching this before it ever reaches
/// kube-rs turns that into an immediate, actionable INVALID_ARGUMENT
/// instead.
fn validate_k8s_name(name: &str) -> Result<(), &'static str> {
    if name.len() > 63 {
        return Err("must be 63 characters or fewer");
    }
    let is_label_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    if !name.chars().all(is_label_char) {
        return Err("must contain only lowercase letters, digits, and '-'");
    }
    let starts_alnum = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    let ends_alnum = name
        .chars()
        .last()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    if !starts_alnum || !ends_alnum {
        return Err("must start and end with a letter or digit");
    }
    Ok(())
}

fn phase_to_proto(phase: ArmaServerPhase) -> ServerPhase {
    match phase {
        ArmaServerPhase::Stopped => ServerPhase::Stopped,
        ArmaServerPhase::Pending => ServerPhase::Pending,
        ArmaServerPhase::Running => ServerPhase::Running,
        ArmaServerPhase::Failed => ServerPhase::Failed,
    }
}

fn desired_state_to_proto(state: DesiredState) -> ProtoDesiredState {
    match state {
        DesiredState::Running => ProtoDesiredState::Running,
        DesiredState::Stopped => ProtoDesiredState::Stopped,
    }
}

/// Operator metadata is stored as annotations under this prefix rather
/// than as an `ArmaServerSpec` field.
///
/// Annotations because nothing reconciles against it -- it exists for
/// humans -- and growing the CRD schema for a free-form map that no
/// controller reads would mean a schema migration for every deployment
/// to gain a field only the UI touches. The prefix keeps it from
/// colliding with the annotations Kubernetes, Helm and kubectl each put
/// on the same object.
const METADATA_PREFIX: &str = "metadata.magpie.skua.io/";

/// Collects into the generated field's own map type (buffa re-exports a
/// HashMap with a different hasher than std's), so this drops straight
/// into `ServerInfo` without a rebuild of the whole map.
fn metadata_from_annotations(obj: &ArmaServer) -> buffa::__private::HashMap<String, String> {
    obj.annotations()
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix(METADATA_PREFIX)
                .map(|k| (k.to_string(), v.clone()))
        })
        .collect()
}

/// Annotation keys must be valid Kubernetes qualified names, so a key
/// that isn't gets rejected here rather than becoming an opaque apply
/// failure from the API server.
fn metadata_to_annotations<'a>(
    entries: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<std::collections::BTreeMap<String, String>, ConnectError> {
    let mut out = std::collections::BTreeMap::new();
    for (key, value) in entries {
        if key.is_empty() {
            return Err(ConnectError::invalid_argument(
                "metadata keys cannot be empty",
            ));
        }
        if !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(ConnectError::invalid_argument(format!(
                "invalid metadata key {key:?}: only letters, digits, '-', '_' and '.' are allowed"
            )));
        }
        out.insert(format!("{METADATA_PREFIX}{key}"), value.to_string());
    }
    Ok(out)
}

fn to_info(obj: &ArmaServer) -> ServerInfo {
    let status = obj.status.clone().unwrap_or_default();
    ServerInfo {
        id: obj.name_any(),
        name: obj.name_any(),
        port: obj.spec.port as u32,
        mod_source_ids: obj.spec.mod_source_ids.clone(),
        phase: EnumValue::Known(phase_to_proto(status.phase)),
        message: status.message,
        desired_state: EnumValue::Known(desired_state_to_proto(obj.spec.desired_state)),
        metadata: metadata_from_annotations(obj),
        ..Default::default()
    }
}

impl protocol::proto::controller::v1::ServerService for ServerServiceImpl {
    async fn create_server<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CreateServerRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ServerInfo> + Send + use<'a>> {
        if request.name.is_empty() {
            return Err(ConnectError::invalid_argument("name is required"));
        }
        if let Err(reason) = validate_k8s_name(&request.name) {
            return Err(ConnectError::invalid_argument(format!(
                "invalid name {:?}: {reason}",
                request.name
            )));
        }
        let port = request.port as u16;
        self.check_port_conflict(port, None).await?;

        let spec = ArmaServerSpec {
            mod_source_ids: request
                .mod_source_ids
                .iter()
                .map(|s| s.to_string())
                .collect(),
            port,
            cdlc: Vec::new(),
            profiling: false,
            desired_state: DesiredState::Running,
            // The ConfigMap named here isn't created/validated by this
            // call at all (gateway has no ConfigMap RBAC) -- it's the
            // caller's responsibility to have it in place first (e.g. via
            // `kubectl edit`, following the same flow `admin arma-config`
            // already uses for the baseline). Unlike leaving this unset,
            // naming a ConfigMap that doesn't actually exist yet is a
            // hard reconcile failure, not a silent baseline-only
            // fallback -- arma_config.rs's fetch_and_merge propagates a
            // missing override ConfigMap as an error.
            config_map: request.config_map.map(|s| s.to_string()),
            metrics: request.metrics_port.map(|port| ArmaServerMetrics {
                port: port as u16,
                path: request.metrics_path.map(|s| s.to_string()),
            }),
            // No CreateServerRequest field for this yet (backend-only
            // for now, see issue #26) -- an operator can still set it
            // directly via `kubectl edit armaserver` once created.
            headless_clients: 0,
        };
        let name = request.name.to_string();

        let annotations = metadata_to_annotations(request.metadata.iter().map(|(k, v)| (*k, *v)))?;

        let obj = ArmaServer {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                annotations: (!annotations.is_empty()).then_some(annotations),
                ..Default::default()
            },
            spec,
            status: None,
        };

        // RegisterSource/Claim/Deployment creation all happen in the
        // reconciler once it observes this object -- this call only needs
        // to make the desired state durable.
        let applied = self
            .api()
            .patch(
                &name,
                &PatchParams::apply("arma-controller").force(),
                &Patch::Apply(&obj),
            )
            .await
            .map_err(|e| {
                ConnectError::internal(format!("failed to apply ArmaServer {name}: {e:#}"))
            })?;

        Response::ok(to_info(&applied))
    }

    async fn delete_server<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, DeleteServerRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<DeleteServerResponse> + Send + use<'a>> {
        // Deliberately does not deregister any of this server's mod
        // sources -- a server going away must not silently stop syncing
        // (or delete, for local sources) content someone may want kept
        // available. Dropping a source is a separate, explicit
        // ModSourceService::DeleteModSource call.
        self.api()
            .delete(request.id, &DeleteParams::default())
            .await
            .map_err(|e| {
                ConnectError::internal(format!("failed to delete ArmaServer {}: {e:#}", request.id))
            })?;
        Response::ok(DeleteServerResponse::default())
    }

    async fn get_server<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetServerRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ServerInfo> + Send + use<'a>> {
        let obj = self
            .api()
            .get(request.id)
            .await
            .map_err(|_| ConnectError::not_found(format!("no such server: {}", request.id)))?;
        Response::ok(to_info(&obj))
    }

    async fn list_servers<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListServersRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListServersResponse> + Send + use<'a>> {
        let list = self
            .api()
            .list(&ListParams::default())
            .await
            .map_err(|e| ConnectError::internal(format!("failed to list: {e:#}")))?;
        let servers = list.items.iter().map(to_info).collect();
        Response::ok(ListServersResponse {
            servers,
            ..Default::default()
        })
    }

    async fn update_server<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, UpdateServerRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ServerInfo> + Send + use<'a>> {
        // Applied before the resync below, not after: resync_and_repending
        // refreshes whatever sources the object references, so changing
        // the set first is what makes newly-attached sources actually get
        // pulled by this same call.
        if request.mod_sources.as_option().is_some() || request.metadata.as_option().is_some() {
            let mut patch = serde_json::Map::new();

            if let Some(selection) = request.mod_sources.as_option() {
                let ids: Vec<String> = selection
                    .mod_source_ids
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                // Every referenced source must exist -- an ArmaServer
                // pointing at a deleted ModSource reconciles into a
                // failure the caller would only find out about later.
                for id in &ids {
                    if self.mod_sources().get(id).await.is_err() {
                        return Err(ConnectError::invalid_argument(format!(
                            "no such mod source: {id}"
                        )));
                    }
                }
                patch.insert("spec".into(), mod_source_ids_patch(ids));
            }

            if let Some(selection) = request.metadata.as_option() {
                let wanted =
                    metadata_to_annotations(selection.metadata.iter().map(|(k, v)| (*k, *v)))?;
                // A merge patch removes a key by setting it to null, so
                // annotations that existed before and aren't in the new
                // set have to be nulled explicitly -- otherwise this
                // would only ever add.
                let existing = self.api().get(request.id).await.map_err(|_| {
                    ConnectError::not_found(format!("no such server: {}", request.id))
                })?;
                let mut annotations = serde_json::Map::new();
                for key in existing.annotations().keys() {
                    if key.starts_with(METADATA_PREFIX) && !wanted.contains_key(key) {
                        annotations.insert(key.clone(), serde_json::Value::Null);
                    }
                }
                for (key, value) in wanted {
                    annotations.insert(key, serde_json::Value::String(value));
                }
                patch.insert(
                    "metadata".into(),
                    serde_json::json!({ "annotations": annotations }),
                );
            }

            self.api()
                .patch(
                    request.id,
                    &PatchParams::default(),
                    &Patch::Merge(serde_json::Value::Object(patch)),
                )
                .await
                .map_err(|e| {
                    ConnectError::internal(format!("failed to update {}: {e:#}", request.id))
                })?;
        }

        let obj = self.resync_and_repending(request.id).await?;
        Response::ok(to_info(&obj))
    }

    async fn get_server_logs<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetServerLogsRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetServerLogsResponse> + Send + use<'a>> {
        let pod = self.server_pod(request.id).await?;
        let name = pod.name_any();

        // Capped rather than trusted: an unbounded tail pulls a whole
        // pod's history through the API server and into a browser tab.
        let tail = request.tail_lines.unwrap_or(500).clamp(1, MAX_LOG_LINES);

        let params = kube::api::LogParams {
            tail_lines: Some(tail as i64),
            previous: request.previous.unwrap_or(false),
            // Deliberately not following: this RPC is unary, so a stream
            // would just block until the request timed out. A live tail
            // wants a streaming RPC of its own.
            follow: false,
            ..Default::default()
        };

        let raw = self.pods().logs(&name, &params).await.map_err(|e| {
            ConnectError::internal(format!("failed to read logs for {name}: {e:#}"))
        })?;

        Response::ok(GetServerLogsResponse {
            lines: raw.lines().map(str::to_string).collect(),
            pod_name: name,
            ..Default::default()
        })
    }

    async fn get_server_health<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetServerHealthRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetServerHealthResponse> + Send + use<'a>> {
        // A stopped server legitimately has no pod. That's "not ready"
        // with an explanation, not an error -- erroring would make the
        // UI show a failure for a server the operator deliberately
        // stopped.
        let Ok(pod) = self.server_pod(request.id).await else {
            return Response::ok(GetServerHealthResponse {
                ready: false,
                message: "no pod is running for this server".to_string(),
                ..Default::default()
            });
        };

        let status = pod.status.clone().unwrap_or_default();
        let phase = status.phase.clone().unwrap_or_default();

        // The Ready condition is exactly the A2S_INFO query probe (see
        // services/launcher/src/healthcheck.rs) -- a real "would a
        // player's server browser see this" answer, not liveness.
        let ready_condition = status
            .conditions
            .unwrap_or_default()
            .into_iter()
            .find(|c| c.type_ == "Ready");
        let ready = ready_condition.as_ref().is_some_and(|c| c.status == "True");

        let container = status
            .container_statuses
            .unwrap_or_default()
            .into_iter()
            .next();
        let restart_count = container.as_ref().map(|c| c.restart_count).unwrap_or(0);

        // Prefer the probe's own explanation, then the pod-level reason,
        // over an empty string that tells an operator nothing.
        let message = ready_condition
            .and_then(|c| c.message)
            .or(status.reason)
            .unwrap_or_else(|| {
                if ready {
                    "answering Steam queries".to_string()
                } else {
                    String::new()
                }
            });

        Response::ok(GetServerHealthResponse {
            ready,
            pod_name: pod.name_any(),
            phase,
            restart_count: restart_count.max(0) as u32,
            message,
            ..Default::default()
        })
    }

    async fn start_server<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, StartServerRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ServerInfo> + Send + use<'a>> {
        let existing = self
            .api()
            .get(request.id)
            .await
            .map_err(|_| ConnectError::not_found(format!("no such server: {}", request.id)))?;
        // Re-checked here, not just at CreateServer time: this server may
        // have been sitting Stopped while another server was created (or
        // started) on an overlapping port range in the meantime.
        self.check_port_conflict(existing.spec.port, Some(request.id))
            .await?;

        let patch = serde_json::json!({ "spec": { "desired_state": "Running" } });
        self.api()
            .patch(request.id, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|e| ConnectError::internal(format!("failed to set desired_state: {e:#}")))?;

        let obj = self.resync_and_repending(request.id).await?;
        Response::ok(to_info(&obj))
    }

    async fn stop_server<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, StopServerRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ServerInfo> + Send + use<'a>> {
        // Graceful stop today just means standard Kubernetes Pod
        // termination (terminationGracePeriodSeconds) once the reconciler
        // sees desired_state flip and deletes the Deployment -- pre-shutdown
        // hooks (e.g. notifying the server over HTTP before it goes down)
        // are planned but not built yet.
        let patch = serde_json::json!({ "spec": { "desired_state": "Stopped" } });
        let obj = self
            .api()
            .patch(request.id, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|e| ConnectError::internal(format!("failed to set desired_state: {e:#}")))?;
        Response::ok(to_info(&obj))
    }
}

/// The `spec` half of the merge patch that replaces a server's mod source
/// selection.
///
/// Its own function purely so the key can be asserted against
/// [`crd::ArmaServerSpec`]'s own serialization in a test. This was
/// `"modSourceIds"` for a while, which no reader and no compiler could
/// catch: `ArmaServerSpec` carries no `rename_all`, so the CRD schema
/// declares `mod_source_ids`, and a structural CRD schema *prunes* unknown
/// fields rather than rejecting them. The patch was therefore accepted with
/// a 200, wrote nothing, left `mod_source_ids` at its old value, and the
/// UpdateServer call went on to resync and re-Pending the server exactly as
/// though it had worked -- so the only visible symptom was a mod source
/// selection that silently reverted on reload.
fn mod_source_ids_patch(ids: Vec<String>) -> serde_json::Value {
    serde_json::json!({ MOD_SOURCE_IDS_FIELD: ids })
}

/// Must match `ArmaServerSpec::mod_source_ids`'s serialized name -- see
/// `mod_source_ids_patch`, and the test that holds the two together.
const MOD_SOURCE_IDS_FIELD: &str = "mod_source_ids";

#[cfg(test)]
mod tests {
    use super::*;

    /// The patch key has to be whatever serde actually emits for that
    /// field, not whatever looked right when it was typed. Deriving the
    /// expectation from a serialized `ArmaServerSpec` rather than
    /// hardcoding the string a second time means adding
    /// `#[serde(rename_all = "camelCase")]` to the spec later fails this
    /// test instead of silently reintroducing the same no-op patch.
    #[test]
    fn patch_key_matches_the_crd_field_name() {
        // Written out rather than `..Default::default()`: ArmaServerSpec
        // has no Default, and giving it one just for a test would let a
        // future field default into a real API call by accident.
        let spec = crd::ArmaServerSpec {
            mod_source_ids: vec!["a".to_string()],
            port: 2302,
            cdlc: Vec::new(),
            profiling: false,
            desired_state: crd::DesiredState::default(),
            config_map: None,
            metrics: None,
            headless_clients: 0,
        };
        let serialized = serde_json::to_value(&spec).expect("spec serializes");
        let object = serialized.as_object().expect("spec is a JSON object");

        assert!(
            object.contains_key(MOD_SOURCE_IDS_FIELD),
            "ArmaServerSpec serializes mod_source_ids as one of {:?}, not {MOD_SOURCE_IDS_FIELD:?}",
            object.keys().collect::<Vec<_>>()
        );
    }

    /// A merge patch only replaces the keys it names, so the patch must
    /// carry the full desired list (and an empty selection has to be an
    /// empty array, not an omitted key -- otherwise "detach every mod
    /// source" would be indistinguishable from "change nothing").
    #[test]
    fn patch_carries_the_whole_selection() {
        let ids = vec!["one".to_string(), "two".to_string()];
        assert_eq!(
            mod_source_ids_patch(ids),
            serde_json::json!({ "mod_source_ids": ["one", "two"] })
        );
        assert_eq!(
            mod_source_ids_patch(Vec::new()),
            serde_json::json!({ "mod_source_ids": [] })
        );
    }
}
