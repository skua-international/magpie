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

    let client = Client::try_default().await?;
    info!("connected to Kubernetes API");

    reconcile::spawn(client, cfg)?;

    // The reconciler runs entirely in its own spawned task; block forever
    // rather than returning immediately.
    std::future::pending::<()>().await;
    Ok(())
}
