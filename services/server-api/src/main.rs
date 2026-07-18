//! `ServerService` only -- a thin, JWT-authenticated frontend for
//! `ArmaServer` CRUD. Deliberately has no ability to create/delete
//! Deployments directly (see its RBAC in the Helm chart): all it does is
//! read/write `ArmaServer` objects through the Kubernetes API, the same
//! surface `services/controller`'s reconciler watches. Least-privilege
//! split from the reconciler -- this is the process an attacker who
//! compromises a bearer token gets to talk to, so it shouldn't be able to
//! touch workloads directly even if the CRD's own RBAC were somehow
//! bypassed.

use std::sync::Arc;

use anyhow::Result;
use authn::authz::{AuthState, require_auth};
use authn::jwt::JwtVerifier;
use axum::routing::get;
use axum::{Router, middleware};
use connectrpc::Router as ConnectRouter;
use kube::Client;
use server_api::config::Config;
use server_api::service::ServerServiceImpl;
use sync_client::SyncClient;
use tracing::info;

fn required_scope(path: &str) -> Option<&'static str> {
    match path {
        "/controller.v1.ServerService/CreateServer" => Some("servers:write"),
        "/controller.v1.ServerService/DeleteServer" => Some("servers:write"),
        "/controller.v1.ServerService/UpdateServer" => Some("servers:write"),
        "/controller.v1.ServerService/StartServer" => Some("servers:write"),
        "/controller.v1.ServerService/StopServer" => Some("servers:write"),
        "/controller.v1.ServerService/ListServers" => Some("servers:read"),
        "/controller.v1.ServerService/GetServer" => Some("servers:read"),
        _ => None,
    }
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

    let pool = registry_db::connect(&cfg.database_url).await?;
    info!("connected to Postgres");

    let client = Client::try_default().await?;
    info!("connected to Kubernetes API");

    let sync_client = SyncClient::new(&cfg.sync_daemon_url)?;
    let server_service = ServerServiceImpl::new(client, cfg.namespace.clone(), sync_client);

    let verifier = JwtVerifier::fetch(&cfg.jwt).await?;
    let auth_state = Arc::new(AuthState {
        verifier,
        pool,
        required_scope,
    });

    let connect = ConnectRouter::new().add_service(server_service);

    let connect_service = tower::ServiceBuilder::new()
        .layer(middleware::from_fn_with_state(auth_state, require_auth))
        .service(connect.into_axum_service());

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .fallback_service(connect_service);

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    info!("listening on {}", cfg.listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
