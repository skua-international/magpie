//! Reads and updates the Steam session Secret (`steam_user`/`refresh_token`
//! keys) -- the chart pre-creates this Secret empty so this process only
//! ever needs `get`/`patch` RBAC on one specific named object
//! (`resourceNames`), never `create`. Kubernetes RBAC's `resourceNames`
//! can't restrict a `create` verb at all (there's no object name to check
//! against yet), so granting this process `create` on Secrets broadly
//! would be a real privilege escalation risk for a comparatively small
//! convenience -- pre-creating the object instead keeps this process's
//! Secret access scoped to exactly one name.

use base64::Engine;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;

pub struct Session {
    pub user: String,
    pub refresh_token: String,
}

/// Reads the session Secret, returning `None` if it's missing either key
/// (including a freshly chart-created empty Secret with no data at all --
/// not an error, just "no session established yet").
pub async fn read_session(client: &Client, namespace: &str, secret_name: &str) -> Option<Session> {
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let secret = api.get(secret_name).await.ok()?;
    let data = secret.data?;
    let user = String::from_utf8(data.get("steam_user")?.0.clone()).ok()?;
    let refresh_token = String::from_utf8(data.get("refresh_token")?.0.clone()).ok()?;
    if user.is_empty() || refresh_token.is_empty() {
        return None;
    }
    Some(Session { user, refresh_token })
}

/// Overwrites the session Secret's data with a freshly established
/// session -- called after any successful login (startup credential
/// negotiation or an interactive RefreshSteamAuth call) that produced a
/// (possibly new) refresh token, so a future restart can skip straight to
/// [`read_session`] instead of needing credentials again.
pub async fn write_session(client: &Client, namespace: &str, secret_name: &str, session: &Session) -> anyhow::Result<()> {
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let b64 = base64::engine::general_purpose::STANDARD;
    let patch = serde_json::json!({
        "data": {
            "steam_user": b64.encode(&session.user),
            "refresh_token": b64.encode(&session.refresh_token),
        }
    });
    api.patch(secret_name, &PatchParams::default(), &Patch::Merge(patch)).await?;
    Ok(())
}
