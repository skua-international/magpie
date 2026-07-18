//! Combines JWT identity verification with a Postgres-backed scope check.
//! Shared by every Connect service in this project (`services/controller`,
//! `services/registry`) -- there is no unauthenticated path anywhere, and
//! no JWT-claim-based authorization (see `jwt.rs`'s doc comment for why).
//! Each service supplies its own RPC-path -> required-scope mapping (they
//! own different procedures), but the verification/lookup/enforcement
//! logic itself is identical everywhere, hence living here instead of
//! being copy-pasted per service.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;

use crate::jwt::JwtVerifier;

#[derive(Clone)]
pub struct AuthIdentity {
    pub subject: String,
    pub scopes: Vec<String>,
}

impl AuthIdentity {
    /// `"*"` is a coarse admin grant (every scope) -- there's no real role
    /// hierarchy yet, just enough to bootstrap an operator account before
    /// anything finer-grained is needed.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == "*" || s == scope)
    }
}

pub struct AuthState {
    pub verifier: JwtVerifier,
    pub pool: PgPool,
    /// Maps a Connect RPC's dispatch path (`/package.Service/Method`) to
    /// the scope required to call it. Anything the function returns `None`
    /// for is denied by default -- a new RPC must be added to the calling
    /// service's map explicitly to become callable.
    pub required_scope: fn(&str) -> Option<&'static str>,
}

pub async fn require_auth(
    State(state): State<Arc<AuthState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(required) = (state.required_scope)(request.uri().path()) else {
        return (StatusCode::NOT_FOUND, "unknown procedure").into_response();
    };

    let Some(token) = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };

    let subject = match state.verifier.verify(token) {
        Ok(subject) => subject,
        Err(msg) => return (StatusCode::UNAUTHORIZED, msg).into_response(),
    };

    let scopes = match registry_db::scopes_for_subject(&state.pool, &subject).await {
        Ok(scopes) => scopes,
        Err(e) => {
            tracing::error!("failed to look up scopes for {subject}: {e:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "authorization check failed",
            )
                .into_response();
        }
    };

    let identity = AuthIdentity { subject, scopes };
    if !identity.has_scope(required) {
        return (
            StatusCode::FORBIDDEN,
            format!("missing required scope: {required}"),
        )
            .into_response();
    }

    request.extensions_mut().insert(identity);
    next.run(request).await
}
