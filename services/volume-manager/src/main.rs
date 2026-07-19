mod blob;
mod config;
mod service;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use blob::BlobManager;
use config::Config;
use connectrpc::Router as ConnectRouter;
use service::VolumeManagerImpl;
use tracing::info;

/// Compares the caller's bearer token against the shared secret both this
/// pod and sync-daemon's Secret carry (see charts/magpie's
/// volume-manager-secret.yaml). This service's NetworkPolicy already
/// restricts ingress to sync-daemon's pod labels, so this is defense in
/// depth, not the only barrier -- but the only barrier at all isn't good
/// enough for the one component in this stack with host block-device
/// privilege.
async fn require_bearer_token(
    State(expected): State<Arc<String>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(token) = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };

    if !constant_time_eq(token.as_bytes(), expected.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "invalid bearer token").into_response();
    }

    next.run(request).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cfg = Config::from_env()?;

    let blob = BlobManager::new(
        cfg.blob_image_path,
        cfg.blob_mount_path,
        cfg.initial_size_bytes,
    );
    let volume_service = std::sync::Arc::new(VolumeManagerImpl::new(blob));
    let connect = ConnectRouter::new().add_service(volume_service);

    let auth_token = Arc::new(cfg.auth_token);
    let connect_service = tower::ServiceBuilder::new()
        .layer(middleware::from_fn_with_state(
            auth_token,
            require_bearer_token,
        ))
        .service(connect.into_axum_service());

    // /healthz deliberately stays outside the auth layer -- health checks
    // shouldn't need a bearer token.
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .fallback_service(connect_service);

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    info!("listening on {}", cfg.listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
