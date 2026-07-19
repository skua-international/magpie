mod config;
mod reconcile;
mod secrets;
mod service;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use config::{Config, SteamAuthConfig};
use connectrpc::Router as ConnectRouter;
use kube::Client;
use secrets::Session;
use service::{Shared, SyncServiceImpl};
use steam_sync::cache::SyncState;
use steam_sync::steam::{CmPool, SteamAuth};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cfg = Config::from_env()?;

    std::fs::create_dir_all(&cfg.content_root)?;
    std::fs::create_dir_all(&cfg.claims_root)?;

    let sync_state = SyncState::open(&cfg.content_root)?;

    let client = Client::try_default().await?;
    info!("connected to Kubernetes API");

    // A stored session always wins over ANONYMOUS_LOGIN -- once
    // established via a RefreshSteamAuth call, the Secret is the source
    // of truth, so a redeploy never re-negotiates against Steam as long
    // as it's valid. There's no credentials-at-startup path at all (see
    // SteamAuthConfig's own doc) -- a password never reaches this process.
    let stored_session =
        secrets::read_session(&client, &cfg.namespace, &cfg.steam_session_secret_name).await;
    let auth = match (&stored_session, &cfg.steam_auth) {
        (Some(session), _) => Some(SteamAuth::Session {
            user: session.user.clone(),
            refresh_token: session.refresh_token.clone(),
        }),
        (None, SteamAuthConfig::Anonymous) => Some(SteamAuth::Anonymous),
        (None, SteamAuthConfig::None) => None,
    };

    // No session, no credentials: start with no Steam connection pool at
    // all rather than refusing to boot -- the HTTP server (and its
    // RefreshSteamAuth RPC) still needs to come up so an operator can
    // establish one without needing to touch this Pod's env/Secrets
    // directly. A CmPool::start failure (bad/expired credentials, Steam
    // Guard now required, transient network issue at boot) degrades the
    // same way rather than crash-looping the whole process.
    let pool = match auth {
        None => {
            warn!(
                "no Steam session established and no credentials configured -- starting with no Steam connection pool. Call RefreshSteamAuth to establish one."
            );
            None
        }
        Some(auth) => {
            info!("Logging in to Steam ({} connections)...", cfg.pool_size);
            match CmPool::start(cfg.pool_size, &auth, &cfg.content_root).await {
                Ok(pool) => {
                    info!("Logged in to Steam");
                    if let Some((user, refresh_token)) = pool.session() {
                        let session = Session {
                            user: user.to_string(),
                            refresh_token: refresh_token.to_string(),
                        };
                        if let Err(e) = secrets::write_session(
                            &client,
                            &cfg.namespace,
                            &cfg.steam_session_secret_name,
                            &session,
                        )
                        .await
                        {
                            warn!("failed to persist Steam session to Secret: {e:#}");
                        }
                    }
                    Some(pool)
                }
                Err(e) => {
                    error!(
                        "failed to start Steam connection pool, starting in degraded mode with none: {e:#}"
                    );
                    None
                }
            }
        }
    };

    let volume_client = if cfg.volume_manager_url.is_empty() {
        warn!("VOLUME_MANAGER_URL not set -- RegisterSource will skip its pre-emptive grow check");
        None
    } else {
        Some(std::sync::Arc::new(volume_client::VolumeClient::new(
            &cfg.volume_manager_url,
            &cfg.volume_manager_token,
        )?))
    };

    let shared = Shared::new(
        pool,
        sync_state,
        cfg.content_root,
        cfg.claims_root,
        client.clone(),
        cfg.namespace.clone(),
        cfg.steam_session_secret_name.clone(),
        volume_client,
        cfg.volume_grow_grace_bytes,
    );

    reconcile::spawn(
        client,
        cfg.namespace.clone(),
        std::time::Duration::from_secs(cfg.poll_interval_secs),
        shared.clone(),
    );

    let sync_service = SyncServiceImpl::new(shared);
    let connect = ConnectRouter::new().add_service(sync_service);

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    info!("Listening on {}", cfg.listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
