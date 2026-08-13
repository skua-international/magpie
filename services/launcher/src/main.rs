mod config;
mod healthcheck;
mod keys;
mod launch;

use anyhow::Result;
use config::Config;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // `/launcher healthcheck` -- exec'd by the readiness/liveness probes
    // (see reconcile.rs), not the normal launch path at all. Handled
    // before the tracing/env setup below since it needs neither and
    // should stay as fast/quiet as possible (probes run every few
    // seconds for this Pod's whole lifetime).
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck::run();
    }

    // Non-blocking: writes go over a channel to a dedicated writer thread
    // instead of the logging call itself blocking on stdout's I/O.
    // Matters more here than in this repo's other services -- launcher
    // now forwards arma3server's own stdout/stderr line-by-line (see
    // launch.rs's forward_stdout/forward_stderr), which can be a lot
    // busier than launcher's own handful of lifecycle log lines, and
    // none of it should be able to add I/O latency to the async tasks
    // reading the child's pipes. `_guard` has to live for the rest of
    // `main` -- dropping it early stops the writer thread and silently
    // drops whatever's still buffered.
    // Traces and structured logs only -- no /metrics endpoint, unlike
    // every other service here. Arma server Pods are hostNetwork: true,
    // so any port this bound would land on the node's real public
    // interface rather than a Pod IP, and there is no host firewall in
    // front of it. The numbers worth having are already covered: startup
    // phase timings become spans, and CPU/memory/restarts come from
    // cAdvisor and kube-state-metrics via the armaserver=<name> Pod label
    // (which the commit introducing magpie's metrics deliberately chose
    // not to duplicate in-process). An operator's own in-game exporter
    // has its own route in anyway -- ArmaServerSpec.metrics{port,path}.
    //
    // JSON, as before: launcher's logs carry arbitrary text a game server
    // chose to print, not just its own structured lines, and JSON keeps
    // each line one well-formed record no matter what that text contains.
    // `telemetry` has to live for the rest of `main` -- dropping it stops
    // the non-blocking writer and discards whatever is still buffered.
    let _telemetry = observability::init("launcher")?;

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
