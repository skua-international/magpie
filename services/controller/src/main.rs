//! `ArmaServer` reconciler -- no auth surface, no RPC API.
//! `services/gateway` is the only thing that talks to this process
//! for anything Arma-related, and it does so purely by writing/reading
//! `ArmaServer` objects through the Kubernetes API, not by calling into
//! this binary directly. Kept separate from gateway specifically so
//! this process's ServiceAccount needs (create/delete Deployments)
//! never has to be granted to the JWT-authenticated API surface --
//! least privilege, given this is the one process in the stack that
//! actually mutates cluster workloads.
//!
//! The one thing this *does* expose is a plain `/healthz` (see `main`) --
//! confirmed live elsewhere in this chart (magpie-csi) that "the
//! process is up" and "actually healthy" are different things worth
//! distinguishing; this process previously had no probe of any kind.

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use controller::config::Config;
use controller::postgres_bootstrap::{self, AppPostgresConfig};
use controller::reconcile;
use kube::Client;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Non-blocking (writes go over a channel to a dedicated writer
    // thread instead of blocking the logging call on stdout's I/O) and
    // JSON (a single well-formed record per line, machine-parseable) --
    // same pattern as launcher/src/main.rs, rolled out repo-wide for
    // consistency. `_guard` has to live for the rest of `main` --
    // dropping it early stops the writer thread and silently drops
    // whatever's still buffered.
    // Logs, metrics and (when an OTLP endpoint is configured) trace
    // export from one place -- see crates/observability. Arc because the
    // /metrics handler is called repeatedly (axum handlers are Fn, not
    // FnOnce) while `main` keeps its own reference alive: dropping the
    // last one shuts the exporters down and stops the non-blocking log
    // writer, silently discarding whatever is still buffered.
    let telemetry = std::sync::Arc::new(observability::init("controller")?);

    let cfg = Arc::new(Config::from_env()?);

    // Must happen before Client::try_default() -- see
    // registry_db::install_crypto_provider's own doc. This process is
    // the one service that needs a kube::Client before it can call
    // registry_db::connect() itself (postgres_bootstrap reads a Secret
    // through it first), so it can't rely on connect() to install this
    // implicitly the way every other service does.
    registry_db::install_crypto_provider();

    let client = Client::try_default().await?;
    info!("connected to Kubernetes API");

    // One long-lived pool for the whole process -- used once here for
    // the app-role bootstrap, then handed to the reconciler for
    // arma_config's ongoing scope queries (admins[]/filePatchingExceptions[]),
    // rather than opening/dropping a pool for each purpose separately.
    let pool = registry_db::connect(&cfg.database_url).await?;

    postgres_bootstrap::ensure_app_role(
        &client,
        &pool,
        &cfg.namespace,
        AppPostgresConfig {
            role: &cfg.app_postgres_role,
            database: &cfg.app_postgres_database,
            secret_name: &cfg.app_postgres_secret_name,
        },
    )
    .await?;

    reconcile::spawn(client, pool, cfg)?;

    // Only ever bound once every startup step above (kube::Client,
    // Postgres pool, app-role bootstrap) has already succeeded -- so
    // kubelet finding nothing listening is itself a meaningful signal
    // that startup hasn't completed, not just a race to probe too
    // early. No readiness distinction from liveness: this reconciler
    // has no notion of "up but not ready yet" beyond that.
    //
    // /metrics has nothing published to it yet -- this reconciler has no
    // business metrics of its own today (server crash/restart counts
    // deliberately come from kube-state-metrics instead, see the plan
    // this landed under), the route exists so the shape is consistent
    // with every other service and ready for whatever gets added later.
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/metrics",
            get({
                let telemetry = telemetry.clone();
                move || {
                    let telemetry = telemetry.clone();
                    async move { telemetry.render() }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    info!("healthz listening on :8080");
    axum::serve(listener, app).await?;
    Ok(())
}
