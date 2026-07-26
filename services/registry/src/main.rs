use std::sync::Arc;

use anyhow::Result;
use authn::authz::{AuthState, require_auth};
use authn::jwt::JwtVerifier;
use axum::routing::get;
use axum::{Router, middleware};
use connectrpc::Router as ConnectRouter;
use kube::Client;
use metrics_exporter_prometheus::PrometheusBuilder;
use registry::config::Config;
use registry::service::admin::AdminServiceImpl;
use registry::service::mission::MissionServiceImpl;
use registry::service::mod_source::ModSourceServiceImpl;
use sync_client::SyncClient;
use tracing::info;

fn required_scope(path: &str) -> Option<&'static str> {
    match path {
        "/registry.v1.ModSourceService/AddModSource" => Some("mod-sources:write"),
        "/registry.v1.ModSourceService/DeleteModSource" => Some("mod-sources:write"),
        "/registry.v1.ModSourceService/ListModSources" => Some("mod-sources:read"),
        // Same scope as AddModSource -- re-resolve + sync whatever's
        // already desired is relatively harmless, nothing destructive.
        "/registry.v1.ModSourceService/SyncModSource" => Some("mod-sources:write"),
        "/registry.v1.ModSourceService/ListSyncedMods" => Some("mod-sources:read"),
        "/registry.v1.ModSourceService/GetSyncedMod" => Some("mod-sources:read"),
        // Deliberately its own scope, distinct from mod-sources:write --
        // even though it's non-destructive (cache-only, see the RPC's own
        // doc), it's still a "force everyone to re-verify this" lever
        // that's easy to misuse/abuse if handed out as casually as
        // AddModSource.
        "/registry.v1.ModSourceService/InvalidateMod" => Some("mod-sources:invalidate"),
        "/registry.v1.MissionService/UploadMission" => Some("missions:write"),
        "/registry.v1.MissionService/DeleteMission" => Some("missions:write"),
        "/registry.v1.MissionService/ListMissions" => Some("missions:read"),
        "/registry.v1.MissionService/GetMission" => Some("missions:read"),
        "/registry.v1.AdminService/GetDiskUsage" => Some("admin:disk-usage"),
        // Its own scope, deliberately separate from everything else here
        // -- this can authenticate as the cluster's Steam account.
        "/registry.v1.AdminService/RefreshSteamAuth" => Some("admin:steam-auth"),
        "/registry.v1.AdminService/ExportState" => Some("admin:export"),
        "/registry.v1.AdminService/ImportState" => Some("admin:import"),
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

    std::fs::create_dir_all(&cfg.local_content_root)?;

    let pool = registry_db::connect(&cfg.database_url).await?;
    info!("connected to Postgres");

    let client = Client::try_default().await?;
    info!("connected to Kubernetes API");

    let prometheus_handle = PrometheusBuilder::new().install_recorder()?;

    let sync_client = Arc::new(SyncClient::new(&cfg.sync_daemon_url)?);
    registry::metrics::spawn(
        client.clone(),
        cfg.namespace.clone(),
        pool.clone(),
        sync_client.clone(),
    );

    let mod_source_service = ModSourceServiceImpl::new(
        client.clone(),
        cfg.namespace.clone(),
        sync_client.clone(),
        cfg.local_content_root.clone().into(),
    );
    let mission_service =
        MissionServiceImpl::new(pool.clone(), cfg.local_content_root.clone().into());
    let admin_service = AdminServiceImpl::new(
        pool.clone(),
        sync_client,
        client,
        cfg.namespace.clone(),
        cfg.armaserver_namespace.clone(),
        cfg.arma_config_baseline.clone(),
    );

    let verifier = JwtVerifier::fetch(&cfg.jwt).await?;
    let auth_state = Arc::new(AuthState {
        verifier,
        pool,
        required_scope,
    });

    let connect = ConnectRouter::new()
        .add_service(mod_source_service)
        .add_service(mission_service)
        .add_service(admin_service);

    // /healthz deliberately stays outside the auth layer -- health checks
    // shouldn't need a bearer token.
    let connect_service = tower::ServiceBuilder::new()
        .layer(middleware::from_fn_with_state(auth_state, require_auth))
        .service(connect.into_axum_service());

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/metrics",
            get(|| async move { prometheus_handle.render() }),
        )
        .fallback_service(connect_service);

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    info!("listening on {}", cfg.listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
