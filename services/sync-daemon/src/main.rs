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
use metrics_exporter_prometheus::PrometheusBuilder;
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

    let sync_state = SyncState::open(&cfg.content_root)?;

    // rustls 0.23 needs a process-level CryptoProvider installed before
    // its first use, or any TLS handshake through it panics -- confirmed
    // live ("Could not automatically determine the process-level
    // CryptoProvider"). Other services get this for free from
    // registry_db::connect() (see its own doc), but sync-daemon doesn't
    // depend on registry-db at all (SQLite-backed, not Postgres), so it
    // never got that install call -- this is the same fix controller's
    // own main.rs needed for the same reason.
    let _ = rustls::crypto::ring::default_provider().install_default();

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

    let shared = Shared::new(
        pool,
        sync_state,
        cfg.content_root,
        client.clone(),
        cfg.namespace.clone(),
        cfg.steam_session_secret_name.clone(),
    );

    reconcile::spawn(
        client,
        cfg.namespace.clone(),
        std::time::Duration::from_secs(cfg.poll_interval_secs),
        shared.clone(),
    );

    // Warm the golden tree (base game + CDLC, plus anything already
    // registered) as soon as this process comes up, instead of only ever
    // syncing lazily on the first server start or an operator's manual
    // SyncModSource call. Spawned, not awaited -- a cold cluster's first
    // download can take a while and shouldn't block readiness. Degrades
    // to a logged warning (not a crash) with no Steam session yet, same
    // as everywhere else in this file.
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            info!("syncing game files at startup");
            match shared.sync_content().await {
                Ok(()) => info!("startup content sync complete"),
                Err(e) => warn!("startup content sync failed: {e:#}"),
            }
        });
    }

    let sync_service = SyncServiceImpl::new(shared);
    let connect = ConnectRouter::new().add_service(sync_service);

    // No require_auth layer here at all (internal-only service, see this
    // file's own doc), so the RPC counters that middleware records for
    // registry/server-api don't apply -- /metrics exists regardless, for
    // shape consistency and whatever gets added here later.
    let prometheus_handle = PrometheusBuilder::new().install_recorder()?;
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/metrics",
            get(|| async move { prometheus_handle.render() }),
        )
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    info!("Listening on {}", cfg.listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
