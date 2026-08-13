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
use gateway::config::Config;
use gateway::service::ServerServiceImpl;
use kube::Client;
use tower_http::services::{ServeDir, ServeFile};
use metrics_exporter_prometheus::PrometheusBuilder;
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
    // Non-blocking + JSON -- same pattern as launcher/src/main.rs,
    // rolled out repo-wide for consistency. `_guard` has to live for the
    // rest of `main` -- dropping it early stops the writer thread and
    // silently drops whatever's still buffered.
    let (non_blocking_stdout, _guard) = tracing_appender::non_blocking(std::io::stdout());
    tracing_subscriber::fmt()
        .json()
        .with_writer(non_blocking_stdout)
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

    let prometheus_handle = PrometheusBuilder::new().install_recorder()?;
    gateway::metrics::spawn(client.clone(), cfg.namespace.clone());

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

    let mut app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/metrics",
            get(|| async move { prometheus_handle.render() }),
        )
        .fallback_service(connect_service);

    // Mounted at /ui rather than taking "/" because "/" is already the
    // catch-all that reaches the Connect services -- claiming it for the
    // UI would mean enumerating every RPC path explicitly, and moving
    // /healthz and /metrics too. /ui is one prefix that needs no other
    // routing to change, in this router or in the chart's Ingress (which
    // routes the whole of "/" here already, so /ui needs no entry there
    // at all).
    //
    // Deliberately NOT behind require_auth: the SPA has to load before
    // anyone can log in through it, and these are public static assets --
    // every byte of data behind them still goes through an authenticated
    // RPC. Authenticating the bundle itself would be a login page that
    // requires being logged in to fetch.
    if let Some(ui_dir) = &cfg.ui_dir {
        // index.html as the fallback, not a 404: the SPA does its own
        // client-side routing, so a deep link or a refresh on /ui/servers
        // has to return the app shell for any path that isn't a real
        // file, and let the router sort it out.
        let index = ui_dir.join("index.html");
        let serve = ServeDir::new(ui_dir).fallback(ServeFile::new(index));
        app = app.nest_service("/ui", serve);
        info!("serving web UI from {}", ui_dir.display());
    } else {
        info!("no UI_DIR configured (or it does not exist) -- not serving a web UI");
    }

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    info!("listening on {}", cfg.listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
