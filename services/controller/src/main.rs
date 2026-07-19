//! `ArmaServer` reconciler only -- no external listener, no auth surface.
//! `services/server-api` is the only thing that talks to this process, and
//! it does so purely by writing/reading `ArmaServer` objects through the
//! Kubernetes API, not by calling into this binary directly. Kept separate
//! from server-api specifically so this process's ServiceAccount needs
//! (create/delete Deployments) never has to be granted to the
//! JWT-authenticated API surface -- least privilege, given this is the one
//! process in the stack that actually mutates cluster workloads.

use std::sync::Arc;

use anyhow::Result;
use controller::config::Config;
use controller::postgres_bootstrap::{self, AppPostgresConfig};
use controller::reconcile;
use kube::Client;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

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

    postgres_bootstrap::ensure_app_role(
        &client,
        &cfg.namespace,
        AppPostgresConfig {
            database_url: &cfg.database_url,
            role: &cfg.app_postgres_role,
            database: &cfg.app_postgres_database,
            secret_name: &cfg.app_postgres_secret_name,
        },
    )
    .await?;

    reconcile::spawn(client, cfg)?;

    // The reconciler runs entirely in its own spawned task; block forever
    // rather than returning immediately.
    std::future::pending::<()>().await;
    Ok(())
}
