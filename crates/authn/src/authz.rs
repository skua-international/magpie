//! Combines JWT identity verification with a Postgres-backed scope check.
//! Shared by every Connect service in this project (`services/controller`,
//! `services/registry`) -- there is no unauthenticated path anywhere, and
//! no JWT-claim-based authorization (see `jwt.rs`'s doc comment for why).
//! Each service supplies its own RPC-path -> required-scope mapping (they
//! own different procedures), but the verification/lookup/enforcement
//! logic itself is identical everywhere, hence living here instead of
//! being copy-pasted per service.

use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, UpDownCounter};
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

/// Records `magpie_rpc_requests_total{path, status}` -- `status` is a
/// coarse class (`401`/`403`/`404`/`500`/`2xx`), not a raw HTTP code
/// (Connect's own RPC-level error codes live in the response body, not
/// necessarily the HTTP status, but every early-return path here still
/// maps to a distinct real HTTP status worth breaking out on its own).
fn record_outcome(path: &str, status: &'static str) {
    RPC_REQUESTS.add(
        1,
        &[
            KeyValue::new("path", path.to_string()),
            KeyValue::new("status", status),
        ],
    );
}

/// Instruments are built once and reused. Creating one per request would
/// re-resolve it against the meter provider on every call, on the request
/// path -- the `metrics` macros this replaces cached for the same reason.
///
/// Constructed lazily rather than in `observability::init` so this crate
/// stays usable by a caller that hasn't set a meter provider up: the SDK
/// falls back to a no-op provider, and the middleware still works.
static RPC_REQUESTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    observability::meter()
        .u64_counter("magpie_rpc_requests_total")
        .with_description("RPCs handled, by path and coarse outcome class")
        .build()
});

/// In-flight is an UpDownCounter, not a gauge: it is incremented and
/// decremented around each request from many tasks at once, which is
/// exactly what that instrument is for. A plain gauge would need one
/// writer holding the true count.
static RPC_IN_FLIGHT: LazyLock<UpDownCounter<i64>> = LazyLock::new(|| {
    observability::meter()
        .i64_up_down_counter("magpie_rpc_requests_in_flight")
        .with_description("RPCs currently being handled, by path")
        .build()
});

pub async fn require_auth(
    State(state): State<Arc<AuthState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();

    let Some(required) = (state.required_scope)(&path) else {
        record_outcome(&path, "404");
        return (StatusCode::NOT_FOUND, "unknown procedure").into_response();
    };

    let Some(token) = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        record_outcome(&path, "401");
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };

    let subject = match state.verifier.verify(token) {
        Ok(subject) => subject,
        Err(msg) => {
            record_outcome(&path, "401");
            return (StatusCode::UNAUTHORIZED, msg).into_response();
        }
    };

    let scopes = match registry_db::scopes_for_subject(&state.pool, &subject).await {
        Ok(scopes) => scopes,
        Err(e) => {
            tracing::error!("failed to look up scopes for {subject}: {e:#}");
            record_outcome(&path, "500");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "authorization check failed",
            )
                .into_response();
        }
    };

    let identity = AuthIdentity { subject, scopes };
    if !identity.has_scope(required) {
        record_outcome(&path, "403");
        return (
            StatusCode::FORBIDDEN,
            format!("missing required scope: {required}"),
        )
            .into_response();
    }

    request.extensions_mut().insert(identity);

    let in_flight_labels = [KeyValue::new("path", path.clone())];
    RPC_IN_FLIGHT.add(1, &in_flight_labels);
    let response = next.run(request).await;
    // Decremented on every path out of here, including a panic-free
    // early return from the handler -- `next.run` either returns a
    // response or unwinds, and an unwind takes the whole task down
    // anyway.
    RPC_IN_FLIGHT.add(-1, &in_flight_labels);

    record_outcome(
        &path,
        if response.status().is_success() {
            "2xx"
        } else {
            "5xx"
        },
    );
    response
}

/// Every scope this cluster's services actually enforce, in the order a
/// UI should offer them.
///
/// Lives here rather than in either service because the set spans both:
/// `servers:*` is enforced by gateway, everything else by registry, and
/// `arma:*` by neither -- those two are read straight out of the grant
/// table by services/controller to populate a server's `admins[]` and
/// `filePatchingExceptions[]`, so they're real grants with no RPC behind
/// them. A list assembled from any one service's `required_scope` table
/// would silently omit the others.
///
/// `"*"` is deliberately absent: it means "every scope", but no single
/// RPC requires it, so it isn't a choice to offer alongside the rest --
/// `AdminService.SetAclScopes` accepts it as a special case.
///
/// Adding a scope to a `required_scope` table without adding it here
/// leaves it ungrantable through any UI, so the two have to move
/// together.
pub const KNOWN_SCOPES: &[&str] = &[
    "servers:read",
    "servers:write",
    "servers:logs",
    "mod-sources:read",
    "mod-sources:write",
    "mod-sources:invalidate",
    "missions:read",
    "missions:write",
    "admin:disk-usage",
    "admin:steam-auth",
    "admin:export",
    "admin:import",
    // Can change who holds anything above, including granting "*".
    "admin:acl",
    // Read/write Secrets in the user-secrets namespace only.
    "admin:secrets",
    // No RPC: read from the grant table by services/controller when
    // rendering main.cfg.
    "arma:admin",
    "arma:filepatch",
];
