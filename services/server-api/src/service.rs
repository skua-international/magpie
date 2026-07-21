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
use protocol::proto::controller::v1::{
    CreateServerRequest, DeleteServerRequest, DeleteServerResponse,
    DesiredState as ProtoDesiredState, GetServerRequest, ListServersRequest, ListServersResponse,
    ServerInfo, ServerPhase, StartServerRequest, StopServerRequest, UpdateServerRequest,
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
            // call at all (server-api has no ConfigMap RBAC) -- it's the
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

        let obj = ArmaServer {
            metadata: ObjectMeta {
                name: Some(name.clone()),
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
        let obj = self.resync_and_repending(request.id).await?;
        Response::ok(to_info(&obj))
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
