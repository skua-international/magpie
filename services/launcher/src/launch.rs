use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use regex::Regex;
use tokio::process::Command as TokioCommand;
use tokio::signal::unix::{SignalKind, signal};

use crate::config::Config;

const SERVER_ROOT: &str = "/arma3/server";
const TMP_CONFIG: &str = "/tmp/arma3.cfg";

fn mod_param(name: &str, mods: &[String]) -> String {
    if mods.is_empty() {
        return String::new();
    }
    format!(" -{}=\"{}\" ", name, mods.join(";"))
}

fn env_defined(key: &str) -> bool {
    std::env::var(key).map(|v| !v.is_empty()).unwrap_or(false)
}

/// Parse an Arma config's `key = value` lines the same loose way the old
/// Python regex did, keyed lowercase for case-insensitive lookups.
fn parse_config_values(data: &str) -> HashMap<String, String> {
    // Mirrors the original: r"(.+?)(?:\s+)?=(?:\s+)?(.+?)(?:$|\/|;)" with MULTILINE.
    let re = Regex::new(r"(?m)(.+?)(?:\s+)?=(?:\s+)?(.+?)(?:$|/|;)").unwrap();
    re.captures_iter(data)
        .map(|c| (c[1].trim().to_lowercase(), c[2].trim().to_string()))
        .collect()
}

/// Build the launch command, spawn headless clients, and exec the server —
/// the Rust port of `launch.py`. `process_start` is captured in `main()`
/// for the final "ready in" log; unlike before this rework, that's the
/// whole story timing-wise -- no login/resolve/download phases happen in
/// this process any more, sync-daemon already did all of that before this
/// run's claim existed.
pub async fn run(cfg: &Config, mods: Vec<String>, process_start: std::time::Instant) -> Result<()> {
    // Clean up previous HC command scripts.
    for entry in glob_hc_scripts()? {
        std::fs::remove_file(&entry)
            .with_context(|| format!("failed to remove old HC script {}", entry.display()))?;
        tracing::info!("Removed old HC script: {}", entry.display());
    }

    let mut launch = format!(
        "{} -limitFPS={} -world={} {} {}",
        cfg.arma_binary,
        std::env::var("ARMA_LIMITFPS").unwrap_or_else(|_| "1000".into()),
        std::env::var("ARMA_WORLD").unwrap_or_else(|_| "empty".into()),
        std::env::var("ARMA_PARAMS").unwrap_or_default(),
        mod_param("mod", &mods),
    );

    for cdlc in &cfg.arma_cdlc {
        launch.push_str(&format!(" -mod={cdlc}"));
    }

    let clients: u32 = std::env::var("HEADLESS_CLIENTS")
        .unwrap_or_else(|_| "0".into())
        .parse()
        .context("HEADLESS_CLIENTS must be an integer")?;
    tracing::info!("Headless Clients: {clients}");

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

    if clients != 0 {
        let config_path = format!("{SERVER_ROOT}/configs/{config_file}");
        let mut data = std::fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {config_path}"))?;

        let config_values = parse_config_values(&data);

        if !config_values.contains_key("headlessclients[]") {
            data.push_str("\nheadlessclients[] = {\"127.0.0.1\"};\n");
        }
        if !config_values.contains_key("localclient[]") {
            data.push_str("\nlocalclient[] = {\"127.0.0.1\"};\n");
        }

        std::fs::write(TMP_CONFIG, &data).context("failed to write temp config")?;
        launch.push_str(&format!(" -config=\"{TMP_CONFIG}\""));

        let mut client_launch = launch.clone();
        client_launch.push_str(&format!(" -client -connect=127.0.0.1 -port={port}"));
        if let Some(password) = config_values.get("password") {
            client_launch.push_str(&format!(" -password={password}"));
        }

        let hc_profile_template =
            std::env::var("HEADLESS_CLIENTS_PROFILE").unwrap_or_else(|_| "$profile-hc-$i".into());
        let arma_profile = std::env::var("ARMA_PROFILE").unwrap_or_else(|_| "main".into());

        for i in 0..clients {
            // Substitute $ii before $i, since $i is a prefix of $ii.
            let hc_name = hc_profile_template
                .replace("$profile", &arma_profile)
                .replace("$ii", &(i + 1).to_string())
                .replace("$i", &i.to_string());

            let hc_launch = format!("{client_launch} -name=\"{hc_name}\"");

            let hc_script_path = format!("{SERVER_ROOT}/hc_command_{}.sh", i + 1);
            std::fs::write(
                &hc_script_path,
                format!("#!/bin/bash\ncd {SERVER_ROOT}\n{hc_launch}\n"),
            )
            .with_context(|| format!("failed to write {hc_script_path}"))?;
            std::fs::set_permissions(
                &hc_script_path,
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )?;
            tracing::info!("Saved HC command to: {hc_script_path}");

            tracing::info!("LAUNCHING ARMA CLIENT {i} WITH {hc_launch}");
            Command::new("sh")
                .arg("-c")
                .arg(&hc_launch)
                .spawn()
                .with_context(|| format!("failed to spawn headless client {i}"))?;
        }
    }

    // INIT_MISSION: auto-start a mission on server launch.
    if env_defined("INIT_MISSION") {
        let mut init_mission = std::env::var("INIT_MISSION").unwrap();
        if let Some(stripped) = init_mission.strip_suffix(".pbo") {
            init_mission = stripped.to_string();
        }
        tracing::info!("INIT_MISSION set to: {init_mission}");

        let mut data = if launch.contains(&format!("-config=\"{TMP_CONFIG}\"")) {
            std::fs::read_to_string(TMP_CONFIG)?
        } else {
            std::fs::read_to_string(format!("{SERVER_ROOT}/configs/{config_file}"))?
        };

        if !data.to_lowercase().contains("persistent") {
            data.push_str("\npersistent = 1;\n");
        }
        data.push_str(&format!(
            "\nclass Missions {{\n  class Mission_1 {{\n    template = \"{init_mission}\";\n    difficulty = \"skua_difficulty\";\n  }};\n}};\n"
        ));

        std::fs::write(TMP_CONFIG, data).context("failed to write temp config for INIT_MISSION")?;
        launch.push_str(" -autoInit");
    }

    if !launch.contains(&format!("-config=\"{TMP_CONFIG}\"")) {
        launch.push_str(&format!(" -config=\"{SERVER_ROOT}/configs/{config_file}\""));
    }

    let arma_profile = std::env::var("ARMA_PROFILE").unwrap_or_else(|_| "main".into());
    let network_config = std::env::var("NETWORK_CONFIG").context("missing NETWORK_CONFIG")?;
    launch.push_str(&format!(
        " -port={port} -name=\"{arma_profile}\" -profiles=\"{SERVER_ROOT}/configs/profiles\" -cfg=\"{SERVER_ROOT}/configs/{network_config}\""
    ));

    tracing::info!("LAUNCHING ARMA SERVER WITH {launch}");
    // `exec` (not a plain `sh -c "<launch>"`, which leaves arma3server as
    // sh's own child) so the spawned process's PID *is* arma3server's --
    // sh replaces its own process image rather than forking, so a signal
    // sent to this PID always reaches arma3server directly regardless of
    // which instant it's sent (before or after the exec syscall
    // completes; the PID is stable across it).
    let mut child = TokioCommand::new("sh")
        .arg("-c")
        .arg(format!("exec {launch}"))
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

fn glob_hc_scripts() -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let dir = Path::new(SERVER_ROOT);
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("hc_command_") && name.ends_with(".sh") {
            out.push(entry.path());
        }
    }
    Ok(out)
}
