mod config;
mod poller;
mod service;

use anyhow::Result;
use axum::routing::get;
use axum::Router;
use config::Config;
use connectrpc::Router as ConnectRouter;
use service::{Shared, SyncServiceImpl};
use steam_sync::cache::SyncState;
use steam_sync::steam::CmPool;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();

    let cfg = Config::from_env()?;

    std::fs::create_dir_all(&cfg.content_root)?;
    std::fs::create_dir_all(&cfg.claims_root)?;

    let sync_state = SyncState::open(&cfg.content_root)?;

    info!("Logging in to Steam ({} connections)...", cfg.pool_size);
    let pool = CmPool::start(cfg.pool_size, &cfg.steam_auth, &cfg.content_root).await?;
    info!("Logged in to Steam");

    let shared = Shared::new(pool, sync_state, cfg.content_root, cfg.claims_root);

    poller::spawn(shared.clone(), std::time::Duration::from_secs(cfg.poll_interval_secs));

    let sync_service = SyncServiceImpl::new(shared);
    let connect = ConnectRouter::new().add_service(sync_service);

    let app = Router::new().route("/healthz", get(|| async { "ok" })).fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    info!("Listening on {}", cfg.listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
