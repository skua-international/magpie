mod capacity;
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

// Only set when built with `--features dhat-heap` (a throwaway profiling
// build, never the production image) -- dhat's Profiler flushes its
// allocation trace to disk on Drop, which requires main() to actually
// return normally. Without with_graceful_shutdown below, kubelet's SIGTERM
// just gets SIGKILLed after the grace period with no unwind at all, so a
// profiling run would silently produce no output.
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

// Production allocator: mimalloc's per-thread heaps/sharded free lists handle
// this daemon's high-frequency concurrent alloc/free pattern (confirmed via
// dhat profiling: 46.7M allocations in one resync, much of it hyper's
// internal per-chunk plumbing across tokio's multi-threaded worker pool)
// better than glibc's default allocator, and its own background purging
// returns idle memory to the OS automatically -- no manual malloc_trim
// needed. Mutually exclusive with dhat-heap above (both are global
// allocators); profiling builds keep dhat's own so allocation tracking
// still works.
#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::builder()
        .file_name(
            std::env::var("DHAT_OUT_PATH").unwrap_or_else(|_| "/content/dhat-heap.json".into()),
        )
        .build();

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

    std::fs::create_dir_all(&cfg.content_root)?;

    let sync_state = SyncState::open(&cfg.content_root).await?;

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
            // Per-slot retry/backoff lives inside CmPool::start itself (see
            // login_with_retry in steam.rs) -- a transient rejection on one
            // connection no longer sacrifices the other already-successful
            // slots by failing the whole batch, so this only needs to
            // handle the case where every slot exhausted its own retries.
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

    // A bad URL here is a config error worth surfacing, but not worth
    // refusing to start over: without a reservation client sync-daemon
    // still downloads correctly, it just leaves magpie-csi's watchdog to
    // notice the space being consumed rather than being told first.
    let capacity = match cfg.csi_capacity_url.as_deref() {
        Some(url) => match capacity::CapacityClient::new(url) {
            Ok(client) => {
                info!("capacity reservations enabled, using {url}");
                Some(std::sync::Arc::new(client))
            }
            Err(e) => {
                warn!("invalid CSI_CAPACITY_URL {url:?} ({e:#}) -- continuing without capacity reservations");
                None
            }
        },
        None => {
            warn!(
                "CSI_CAPACITY_URL not set -- continuing without capacity reservations; magpie-csi's watchdog is the only defense against the blob filling mid-sync"
            );
            None
        }
    };

    let shared = Shared::new(
        pool,
        sync_state,
        cfg.content_root,
        client.clone(),
        cfg.namespace.clone(),
        cfg.steam_session_secret_name.clone(),
        cfg.download_workers,
        capacity,
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

    // Passive background re-sync on a timer, on top of the explicit
    // triggers above (RPC, a server starting, a ModSource's first
    // resolve) -- otherwise content nothing has explicitly touched in a
    // while (a server sitting Stopped, a mod source nobody's restarted
    // against) can silently drift out of date against Steam's current
    // manifests indefinitely. `interval.tick()` fires immediately on its
    // own first call, which would just duplicate the startup sync above
    // -- skipped with one throwaway tick before entering the loop.
    if cfg.content_sync_interval_secs > 0 {
        let shared = shared.clone();
        let period = std::time::Duration::from_secs(cfg.content_sync_interval_secs);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.tick().await;
            loop {
                interval.tick().await;
                info!("running scheduled background content sync");
                match shared.sync_content().await {
                    Ok(()) => info!("scheduled content sync complete"),
                    Err(e) => warn!("scheduled content sync failed: {e:#}"),
                }
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
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

/// Resolves on SIGTERM (what kubelet sends on pod deletion/scale-down) or
/// Ctrl+C -- lets main() return normally instead of being SIGKILLed after
/// the grace period, which matters for the dhat-heap profiling build (see
/// its own comment above) but is generally correct behavior regardless.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("received shutdown signal, shutting down gracefully");
}
