//! `AdminService`: cluster-wide storage accounting, Steam session
//! refresh, and (see state.rs) declarative cluster state export/import.

use std::sync::Arc;

use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};

use protocol::proto::registry::v1::{
    AclSubject, BeginSteamQrLoginRequest, BeginSteamQrLoginResponse, DeleteSecretRequest,
    DeleteSecretResponse, ExportStateRequest, ExportStateResponse, GetDiskUsageRequest,
    GetDiskUsageResponse, ImportStateRequest, ImportStateResponse, LinkedAccountInfo,
    ListAclRequest, ListAclResponse, ListSecretsRequest, ListSecretsResponse,
    PollSteamQrLoginRequest, PollSteamQrLoginResponse, PutSecretRequest, PutSecretResponse,
    RefreshSteamAuthRequest, RefreshSteamAuthResponse, SecretInfo, SetAclScopesRequest,
    SetAclScopesResponse,
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
    /// See `Config::user_secrets_namespace` -- the only namespace the
    /// secret RPCs below ever touch.
    user_secrets_namespace: String,
}

impl AdminServiceImpl {
    pub fn new(
        pool: PgPool,
        sync_client: Arc<SyncClient>,
        client: Client,
        namespace: String,
        armaserver_namespace: String,
        arma_config_baseline: String,
        user_secrets_namespace: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            sync_client,
            client,
            namespace,
            armaserver_namespace,
            arma_config_baseline,
            user_secrets_namespace,
        })
    }
}

impl AdminServiceImpl {
    /// Namespaced to `user_secrets_namespace` and nowhere else -- the
    /// RBAC granted for this is a Role in that namespace, so any other
    /// namespace would be forbidden anyway; constructing it here makes
    /// that explicit rather than incidental.
    fn secrets(&self) -> Api<Secret> {
        Api::namespaced(self.client.clone(), &self.user_secrets_namespace)
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

    async fn list_acl<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListAclRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListAclResponse> + Send + use<'a>> {
        let rows = registry_db::list_acl_subjects(&self.pool)
            .await
            .map_err(|e| ConnectError::internal(format!("failed to list ACL subjects: {e:#}")))?;
        Response::ok(ListAclResponse {
            subjects: rows.into_iter().map(to_acl_subject).collect(),
            known_scopes: authn::authz::KNOWN_SCOPES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ..Default::default()
        })
    }

    async fn set_acl_scopes<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SetAclScopesRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<SetAclScopesResponse> + Send + use<'a>> {
        if request.subject.trim().is_empty() {
            return Err(ConnectError::invalid_argument("subject is required"));
        }
        // Rejected rather than silently dropped: a scope no RPC enforces
        // grants nothing, so accepting it would show up in the UI as
        // access the holder does not actually have. "*" is allowed
        // explicitly -- it's the coarse full-admin grant, and is
        // deliberately not in KNOWN_SCOPES since no single RPC requires
        // it.
        let scopes: Vec<String> = request.scopes.iter().map(|s| s.to_string()).collect();
        for scope in &scopes {
            if scope != "*" && !authn::authz::KNOWN_SCOPES.contains(&scope.as_str()) {
                return Err(ConnectError::invalid_argument(format!(
                    "unknown scope {scope:?} -- see ListAcl's known_scopes"
                )));
            }
        }

        registry_db::set_scopes(&self.pool, request.subject, &scopes)
            .await
            .map_err(|e| ConnectError::internal(format!("failed to set scopes: {e:#}")))?;

        // Read back rather than echoing the request, so the response
        // reflects what's actually stored (and carries the linked
        // accounts the request never had).
        let rows = registry_db::list_acl_subjects(&self.pool)
            .await
            .map_err(|e| ConnectError::internal(format!("failed to re-read ACL: {e:#}")))?;
        let subject = rows
            .into_iter()
            .find(|r| r.subject == request.subject)
            .map(to_acl_subject);
        Response::ok(SetAclScopesResponse {
            subject: subject.into(),
            ..Default::default()
        })
    }

    async fn list_secrets<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListSecretsRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListSecretsResponse> + Send + use<'a>> {
        let list = self
            .secrets()
            .list(&ListParams::default())
            .await
            .map_err(|e| ConnectError::internal(format!("failed to list secrets: {e:#}")))?;

        let mut secrets: Vec<SecretInfo> = list
            .items
            .into_iter()
            .filter(|s| {
                // Service-account token secrets are Kubernetes' own, not
                // an operator's, and are noise in a UI meant for
                // `secret:` placeholder targets.
                s.type_.as_deref() != Some("kubernetes.io/service-account-token")
            })
            .map(|secret| {
                // Key names only, never values -- see the RPC's proto
                // doc. A list call must not be an exfiltration path.
                let mut keys: Vec<String> = secret
                    .data
                    .unwrap_or_default()
                    .into_keys()
                    .chain(secret.string_data.unwrap_or_default().into_keys())
                    .collect();
                keys.sort_unstable();
                keys.dedup();
                SecretInfo {
                    name: secret.metadata.name.unwrap_or_default(),
                    keys,
                    ..Default::default()
                }
            })
            .collect();
        secrets.sort_by(|a, b| a.name.cmp(&b.name));

        Response::ok(ListSecretsResponse {
            secrets,
            namespace: self.user_secrets_namespace.clone(),
            ..Default::default()
        })
    }

    async fn put_secret<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, PutSecretRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<PutSecretResponse> + Send + use<'a>> {
        let name = request.name.trim();
        validate_secret_name(name)?;

        // string_data rather than data: Kubernetes base64-encodes it
        // server-side, so plaintext never has to be encoded here and a
        // caller can't produce an object whose data doesn't decode.
        let string_data: std::collections::BTreeMap<String, String> = request
            .data
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        for key in string_data.keys() {
            if key.is_empty()
                || !key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err(ConnectError::invalid_argument(format!(
                    "invalid secret key {key:?}: only letters, digits, '-', '_' and '.' are allowed"
                )));
            }
        }

        let obj = Secret {
            metadata: kube::core::ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(self.user_secrets_namespace.clone()),
                ..Default::default()
            },
            string_data: Some(string_data),
            ..Default::default()
        };

        // Server-side apply, so this replaces the secret's data rather
        // than merging into it -- a key the caller dropped is genuinely
        // removed, which is what "Put" has to mean for the UI's editor
        // to be able to delete a key at all.
        let applied = self
            .secrets()
            .patch(
                name,
                &PatchParams::apply("magpie-registry").force(),
                &Patch::Apply(&obj),
            )
            .await
            .map_err(|e| ConnectError::internal(format!("failed to write secret {name}: {e:#}")))?;

        let mut keys: Vec<String> = applied
            .data
            .unwrap_or_default()
            .into_keys()
            .chain(applied.string_data.unwrap_or_default().into_keys())
            .collect();
        keys.sort_unstable();
        keys.dedup();

        Response::ok(PutSecretResponse {
            secret: Some(SecretInfo {
                name: applied.metadata.name.unwrap_or_default(),
                keys,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        })
    }

    async fn delete_secret<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, DeleteSecretRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<DeleteSecretResponse> + Send + use<'a>> {
        let name = request.name.trim();
        validate_secret_name(name)?;
        self.secrets()
            .delete(name, &DeleteParams::default())
            .await
            .map_err(|e| {
                ConnectError::internal(format!("failed to delete secret {name}: {e:#}"))
            })?;
        Response::ok(DeleteSecretResponse::default())
    }

    async fn begin_steam_qr_login<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, BeginSteamQrLoginRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<BeginSteamQrLoginResponse> + Send + use<'a>> {
        // Straight passthrough: the Steam CM connection this opens lives
        // in sync-daemon, which is the process that ultimately needs the
        // session. Nothing about it is held here.
        let (session_id, challenge_url) = self
            .sync_client
            .begin_qr_login()
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(BeginSteamQrLoginResponse {
            session_id,
            challenge_url,
            ..Default::default()
        })
    }

    async fn poll_steam_qr_login<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, PollSteamQrLoginRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<PollSteamQrLoginResponse> + Send + use<'a>> {
        let (confirmed, username) = self
            .sync_client
            .poll_qr_login(request.session_id)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(PollSteamQrLoginResponse {
            confirmed,
            username,
            ..Default::default()
        })
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

/// Zips the three parallel account arrays (see `AclSubjectRow`'s own doc
/// for why they're stored that way) back into one account per index.
fn to_acl_subject(row: registry_db::AclSubjectRow) -> AclSubject {
    let accounts = row
        .providers
        .into_iter()
        .zip(row.provider_user_ids)
        .zip(row.display_names)
        .map(
            |((provider, provider_user_id), display_name)| LinkedAccountInfo {
                provider,
                provider_user_id,
                display_name,
                ..Default::default()
            },
        )
        .collect();
    AclSubject {
        subject: row.subject,
        accounts,
        scopes: row.scopes,
        ..Default::default()
    }
}

/// Kubernetes object names are DNS-1123 subdomains; rejecting here gives
/// a caller a straight answer instead of an opaque apply failure, and
/// keeps a name like "../other" from ever reaching the API server.
fn validate_secret_name(name: &str) -> Result<(), ConnectError> {
    if name.is_empty() || name.len() > 253 {
        return Err(ConnectError::invalid_argument(
            "secret name must be 1-253 characters",
        ));
    }
    let valid = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.'))
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !valid {
        return Err(ConnectError::invalid_argument(format!(
            "invalid secret name {name:?}: lowercase letters, digits, '-' and '.' only, \
             starting and ending with a letter or digit"
        )));
    }
    Ok(())
}
