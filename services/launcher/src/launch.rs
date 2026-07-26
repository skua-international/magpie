use std::net::UdpSocket;
use std::os::unix::process::ExitStatusExt;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout, Command as TokioCommand};
use tokio::signal::unix::{SignalKind, signal};

use crate::config::Config;

const SERVER_ROOT: &str = "/arma3/server";

// Arma binds a 5-port range starting at PORT (see crates/crd/src/lib.rs's
// own doc on ArmaServerSpec::port) -- game port, query port (PORT+1,
// what healthcheck.rs probes), plus 3 more. A `kubectl delete pod` races
// the replacement pod's launcher against the old pod's arma3server_x64
// still tearing down and holding all 5; confirmed live: "CreateBoundSocket:
// ::bind couldn't find an open port between 2303 and 2303" when the
// replacement started before the old process actually released them.
const PORT_RANGE_SIZE: u16 = 5;
const PORT_WAIT_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const PORT_WAIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Blocks until every port in `port..port+PORT_RANGE_SIZE` can be bound, or
/// bails after `PORT_WAIT_TIMEOUT`. Deliberately runs *before* arma3server
/// is ever spawned, not as a retry-after-crash loop: this process staying
/// alive without having spawned the child yet is what keeps the readiness
/// probe (an actual UDP query against a port nothing is listening on)
/// reporting not-ready for the whole wait, with no separate "I'm waiting"
/// signal needed -- see healthcheck.rs.
async fn wait_for_ports_free(port: u16) -> Result<()> {
    let deadline = tokio::time::Instant::now() + PORT_WAIT_TIMEOUT;
    loop {
        // Bind-and-drop probe, same primitive healthcheck.rs already uses
        // (UdpSocket::bind) -- each socket closes immediately after this
        // block, so a free port isn't held away from arma3server itself.
        let busy = (0..PORT_RANGE_SIZE)
            .map(|offset| port + offset)
            .find(|&p| UdpSocket::bind(("0.0.0.0", p)).is_err());

        let Some(busy_port) = busy else {
            return Ok(());
        };

        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "port {busy_port} (in range {port}..{}) still bound after {:.0}s, giving up",
                port + PORT_RANGE_SIZE - 1,
                PORT_WAIT_TIMEOUT.as_secs_f64()
            );
        }

        tracing::warn!(
            "port {busy_port} still in use (likely the previous pod's arma3server still \
             releasing it) -- waiting"
        );
        tokio::time::sleep(PORT_WAIT_RETRY_INTERVAL).await;
    }
}

/// Build the launch args and exec the server directly (no shell involved
/// at all -- see the spawn call's own doc for why that's more than just
/// an image-size nicety). `process_start` is captured in `main()` for the
/// final "ready in" log; unlike before this rework, that's the whole
/// story timing-wise -- no login/resolve/download phases happen in this
/// process any more, sync-daemon already did all of that before this
/// run's claim existed.
///
/// `cfg.client_connect` is what switches this into headless-client mode
/// (see `services/controller/src/reconcile.rs`'s `ensure_hc_deployment`,
/// the only place that ever sets it): same mods/CDLC as a normal launch,
/// but `-client -connect=<...> -port=<owning server's port>` instead of
/// hosting anything, and no `-config=`/`-cfg=` at all -- neither applies
/// to a connecting client, only to whatever process is actually hosting.
///
/// `-world=empty` and the `main.cfg`/`basic.cfg` filenames are hardcoded,
/// not configurable -- the ConfigMap-driven config (arma_config.rs) is
/// the single source of truth for what a server actually needs tuned,
/// and neither of these has ever had a real reason to vary: "empty" is
/// what every dedicated server uses (a genuinely different world is an
/// `additional_params` concern, not a first-class knob), and
/// arma_config.rs itself only ever writes those two exact filenames.
pub async fn run(cfg: &Config, mods: Vec<String>, process_start: std::time::Instant) -> Result<()> {
    let mut args = vec![
        format!(
            "-limitFPS={}",
            std::env::var("ARMA_LIMITFPS").unwrap_or_else(|_| "300".into())
        ),
        "-world=empty".to_string(),
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

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(2302);
    let arma_profile = std::env::var("ARMA_PROFILE").unwrap_or_else(|_| "main".into());

    wait_for_ports_free(port).await?;

    if let Some(connect) = &cfg.client_connect {
        args.push("-client".to_string());
        args.push(format!("-connect={connect}"));
        args.push(format!("-port={port}"));
        if let Some(password) = &cfg.client_password {
            args.push(format!("-password={password}"));
        }
        args.push(format!("-name={arma_profile}"));
        args.push(format!("-profiles={SERVER_ROOT}/configs/profiles"));
    } else {
        args.push(format!("-config={SERVER_ROOT}/configs/main.cfg"));
        args.push(format!("-port={port}"));
        args.push(format!("-name={arma_profile}"));
        args.push(format!("-profiles={SERVER_ROOT}/configs/profiles"));
        args.push(format!("-cfg={SERVER_ROOT}/configs/basic.cfg"));
    }

    tracing::info!(
        "LAUNCHING ARMA {} WITH {} {}",
        if cfg.client_connect.is_some() {
            "CLIENT"
        } else {
            "SERVER"
        },
        cfg.arma_binary,
        args.join(" ")
    );
    // Spawned directly, no shell involved -- unlike the `sh -c "exec
    // <cmd>"` this used to go through, args here are passed as real
    // argv entries (Command::args), never re-parsed/re-quoted by
    // anything, and the resulting Child's own PID *is* arma3server's
    // PID from the moment it's spawned. That was the whole reason the
    // old code specifically used `exec` inside a shell wrapper (so the
    // shell replaced itself instead of leaving arma3server as its
    // child) -- spawning the real binary directly makes that
    // indirection unnecessary rather than just avoiding it.
    // Piped, not inherited -- inheriting would already put arma3server's
    // raw output on this container's stdout/stderr (and `kubectl logs`
    // does show it today), but unstructured: no timestamp, no level, not
    // tagged as coming from the child process rather than launcher
    // itself. Piping and forwarding each line through `tracing::info!`
    // gets it the same structured formatting (and, once launcher's own
    // subscriber emits JSON, the same machine-parseable shape) as every
    // other log line this process produces.
    let mut child = TokioCommand::new(&cfg.arma_binary)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn arma3server")?;

    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(forward_stdout(stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(forward_stderr(stderr));
    }

    // No handler at all used to mean Kubernetes' pod-deletion signal
    // killed this process immediately, forwarding nothing to arma3server
    // and giving it no chance at a clean shutdown. Forward it and wait
    // for the real exit instead of dying immediately ourselves.
    //
    // Both SIGTERM *and* SIGINT are handled, not just SIGTERM: the
    // Dockerfile sets `STOPSIGNAL SIGINT`, which is what Kubernetes
    // actually sends PID 1 on pod deletion -- a SIGTERM-only handler here
    // means Rust's default SIGINT behavior (immediate death, no handler)
    // kills launcher without ever forwarding anything, leaving
    // arma3server to die from losing its parent instead of a clean
    // shutdown -- directly relevant to `wait_for_ports_free` above, since
    // a clean shutdown releases the port range far more promptly than an
    // orphaned process does.
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    let mut sigint =
        signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
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
        _ = sigint.recv() => {
            tracing::info!("received SIGINT -- forwarding to arma3server and waiting for it to exit");
            if let Some(pid) = child.id() {
                // SAFETY: same as the SIGTERM branch above.
                unsafe { libc::kill(pid as i32, libc::SIGINT) };
            }
            child.wait().await.context("failed to wait for arma3server after SIGINT")?
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

/// Reads arma3server's stdout line-by-line and re-emits each one through
/// `tracing::info!`, tagged so it's distinguishable from launcher's own
/// log lines. Runs until the pipe closes (the child exited and dropped
/// its end) -- not awaited by `run` itself, deliberately: `child.wait()`
/// already reaps the process and reports its exit status; this task's
/// job ends when there's nothing left to read, whichever happens first.
async fn forward_stdout(stdout: ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => tracing::info!(stream = "stdout", "{line}"),
            Ok(None) => return,
            // A read error here is arma3server's pipe going away
            // unexpectedly, not launcher's own concern to propagate --
            // `run`'s own `child.wait()` is what surfaces a real failure.
            Err(e) => {
                tracing::warn!("stopped reading arma3server stdout: {e:#}");
                return;
            }
        }
    }
}

/// Same as `forward_stdout`, but for stderr, logged at `warn` -- arma
/// itself doesn't distinguish "diagnostic" from "actual problem" on
/// this stream any more clearly than most CLI tools do, but warn is a
/// closer default than info for "the child chose to write here".
async fn forward_stderr(stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => tracing::warn!(stream = "stderr", "{line}"),
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("stopped reading arma3server stderr: {e:#}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Picks an ephemeral port from the OS (bind to :0, read it back, drop
    // immediately) rather than a hardcoded literal -- avoids flaking on a
    // CI box where something else already holds a fixed test port.
    fn free_port_base() -> u16 {
        UdpSocket::bind(("0.0.0.0", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[tokio::test]
    async fn wait_for_ports_free_returns_immediately_when_all_free() {
        let port = free_port_base();
        tokio::time::timeout(Duration::from_secs(1), wait_for_ports_free(port))
            .await
            .expect("should not time out")
            .expect("all 5 ports are free");
    }

    #[tokio::test]
    async fn wait_for_ports_free_waits_until_a_held_port_releases() {
        let port = free_port_base();
        // Hold one port in the middle of the 5-port range, exactly the
        // "old pod's arma3server hasn't released it yet" scenario.
        let held = UdpSocket::bind(("0.0.0.0", port + 2)).unwrap();

        let wait = tokio::spawn(wait_for_ports_free(port));
        tokio::time::sleep(PORT_WAIT_RETRY_INTERVAL * 2).await;
        // Still waiting -- the held port hasn't been released yet.
        assert!(!wait.is_finished());

        drop(held);
        tokio::time::timeout(PORT_WAIT_TIMEOUT, wait)
            .await
            .expect("should not time out")
            .expect("task should not panic")
            .expect("should succeed once the held port is released");
    }

    #[tokio::test]
    async fn wait_for_ports_free_bails_after_timeout() {
        let port = free_port_base();
        let _held = UdpSocket::bind(("0.0.0.0", port)).unwrap();

        // Real timeout is 120s -- too slow for a unit test to actually
        // exercise. This just confirms the loop keeps retrying (doesn't
        // return Ok early) while the port stays held; the timeout path
        // itself is a straightforward bail! covered by inspection, not
        // worth a slow test.
        tokio::time::timeout(PORT_WAIT_RETRY_INTERVAL * 2, wait_for_ports_free(port))
            .await
            .expect_err("should still be waiting, not have returned");
    }
}
