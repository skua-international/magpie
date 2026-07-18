mod config;
mod keys;
mod launch;

use anyhow::Result;
use config::Config;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let process_start = std::time::Instant::now();
    info!("Starting Arma 3 Server...");

    let cfg = Config::from_env()?;
    let mods: Vec<String> = cfg.mods.clone();

    // No Steam login, no resolve/sync/download -- everything Steam-related
    // is sync-daemon's job now (see steam-sync and services/sync-daemon).
    // This process just consumes an already-synced claim: copy out the
    // .bikey files for whatever mods this run is actually using, then
    // launch.
    keys::ensure_keys_dir()?;
    for mod_path in &cfg.mods {
        let mod_dir = cfg.claim_path.join(mod_path);
        if let Err(e) = keys::copy(&mod_dir) {
            tracing::warn!("failed to copy keys from {}: {e:#}", mod_dir.display());
        }
    }

    launch::run(&cfg, mods, process_start).await
}
