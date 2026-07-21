use std::os::unix::process::ExitStatusExt;

use anyhow::{Context, Result};
use tokio::process::Command as TokioCommand;
use tokio::signal::unix::{SignalKind, signal};

use crate::config::Config;

const SERVER_ROOT: &str = "/arma3/server";
const TMP_CONFIG: &str = "/tmp/arma3.cfg";

fn env_defined(key: &str) -> bool {
    std::env::var(key).map(|v| !v.is_empty()).unwrap_or(false)
}

/// Build the launch args and exec the server directly (no shell involved
/// at all -- see the spawn call's own doc for why that's more than just
/// an image-size nicety). `process_start` is captured in `main()` for the
/// final "ready in" log; unlike before this rework, that's the whole
/// story timing-wise -- no login/resolve/download phases happen in this
/// process any more, sync-daemon already did all of that before this
/// run's claim existed. Headless clients are gone entirely (see the
/// tracked follow-up for real support) -- this only ever launches the
/// one server process now.
pub async fn run(cfg: &Config, mods: Vec<String>, process_start: std::time::Instant) -> Result<()> {
    let mut args = vec![
        format!(
            "-limitFPS={}",
            std::env::var("ARMA_LIMITFPS").unwrap_or_else(|_| "1000".into())
        ),
        format!(
            "-world={}",
            std::env::var("ARMA_WORLD").unwrap_or_else(|_| "empty".into())
        ),
    ];
    // Extra operator-supplied flags, appended verbatim after the
    // generated -mod=/CDLC ones below -- split on whitespace since
    // there's no shell here any more to do that for us. A quoted value
    // containing spaces (e.g. wanting `-name="My Server"` inside this)
    // won't survive that split; nothing in this codebase has ever
    // needed that, and CreateServerRequest's own `params` field is
    // already a list, not a single string, for exactly this reason.
    if let Ok(params) = std::env::var("ARMA_PARAMS") {
        args.extend(params.split_whitespace().map(str::to_string));
    }
    if !mods.is_empty() {
        args.push(format!("-mod={}", mods.join(";")));
    }
    for cdlc in &cfg.arma_cdlc {
        args.push(format!("-mod={cdlc}"));
    }

    println!(
        "\n==========================================\n=                                        =\n=   IT'S LAUNCHING, HAIL TO THE KING     =\n=                                        =\n==========================================\n"
    );
    let now = std::time::Instant::now();
    tracing::info!("Ready in {:.1}s", (now - process_start).as_secs_f64());

    // The arma3 binary and workshop mods live under the claim (Steam-synced
    // content); configs/profiles/keys stay under the separate, fixed
    // SERVER_ROOT referenced by absolute path everywhere below.
    std::env::set_current_dir(&cfg.claim_path)
        .with_context(|| format!("failed to chdir into {}", cfg.claim_path.display()))?;

    let config_file = std::env::var("ARMA_CONFIG").context("missing ARMA_CONFIG")?;
    let port = std::env::var("PORT").unwrap_or_else(|_| "2302".into());

    // INIT_MISSION: auto-start a mission on server launch.
    let mut config_arg = format!("{SERVER_ROOT}/configs/{config_file}");
    if env_defined("INIT_MISSION") {
        let mut init_mission = std::env::var("INIT_MISSION").unwrap();
        if let Some(stripped) = init_mission.strip_suffix(".pbo") {
            init_mission = stripped.to_string();
        }
        tracing::info!("INIT_MISSION set to: {init_mission}");

        let mut data = std::fs::read_to_string(&config_arg)
            .with_context(|| format!("failed to read {config_arg}"))?;

        if !data.to_lowercase().contains("persistent") {
            data.push_str("\npersistent = 1;\n");
        }
        data.push_str(&format!(
            "\nclass Missions {{\n  class Mission_1 {{\n    template = \"{init_mission}\";\n    difficulty = \"skua_difficulty\";\n  }};\n}};\n"
        ));

        std::fs::write(TMP_CONFIG, data).context("failed to write temp config for INIT_MISSION")?;
        config_arg = TMP_CONFIG.to_string();
        args.push("-autoInit".to_string());
    }
    args.push(format!("-config={config_arg}"));

    let arma_profile = std::env::var("ARMA_PROFILE").unwrap_or_else(|_| "main".into());
    let network_config = std::env::var("NETWORK_CONFIG").context("missing NETWORK_CONFIG")?;
    args.push(format!("-port={port}"));
    args.push(format!("-name={arma_profile}"));
    args.push(format!("-profiles={SERVER_ROOT}/configs/profiles"));
    args.push(format!("-cfg={SERVER_ROOT}/configs/{network_config}"));

    tracing::info!("LAUNCHING ARMA SERVER WITH {} {}", cfg.arma_binary, args.join(" "));
    // Spawned directly, no shell involved -- unlike the `sh -c "exec
    // <cmd>"` this used to go through, args here are passed as real
    // argv entries (Command::args), never re-parsed/re-quoted by
    // anything, and the resulting Child's own PID *is* arma3server's
    // PID from the moment it's spawned. That was the whole reason the
    // old code specifically used `exec` inside a shell wrapper (so the
    // shell replaced itself instead of leaving arma3server as its
    // child) -- spawning the real binary directly makes that
    // indirection unnecessary rather than just avoiding it.
    let mut child = TokioCommand::new(&cfg.arma_binary)
        .args(&args)
        .spawn()
        .context("failed to spawn arma3server")?;

    // No handler at all used to mean Kubernetes' SIGTERM (pod deletion,
    // or the Recreate rollover a resync triggers) killed this process
    // immediately, forwarding nothing to arma3server and giving it no
    // chance at a clean shutdown. Forward SIGTERM and wait for the real
    // exit instead of dying immediately ourselves.
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    let status = tokio::select! {
        status = child.wait() => status.context("failed to wait for arma3server")?,
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM -- forwarding to arma3server and waiting for it to exit");
            if let Some(pid) = child.id() {
                // SAFETY: pid is a real, currently-running child of this
                // process (just read from the still-live Child handle) --
                // kill(2) with a valid pid and SIGTERM is always sound.
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            }
            child.wait().await.context("failed to wait for arma3server after SIGTERM")?
        }
    };

    if !status.success() {
        anyhow::bail!(
            "arma3server exited with status {}",
            status
                .code()
                .unwrap_or_else(|| status.signal().unwrap_or(-1))
        );
    }

    Ok(())
}
