//! A local-only operator tool: negotiates a Steam session (interactive
//! username+password, optional Guard code) and prints the resulting
//! refresh token as one line of JSON. Never run as a deployed service --
//! invoked by `magpie admin refresh-steam-auth` (cli/magpie), which owns
//! everything downstream of the negotiation itself (writing the cluster's
//! Secret via kubectl, restarting sync-daemon to pick it up). Lives as a
//! `src/bin/` target on the existing steam-sync crate rather than its own
//! workspace member -- it's a thin wrapper around `negotiate_interactive`,
//! which already lives here, not enough surface to justify a whole
//! separate package.
//!
//! The point of routing this through a local process at all, rather than
//! an RPC to a deployed service (the old design): the password is used
//! only for the duration of this process's own run, sent directly to
//! Steam, and never reaches anything we deploy to the cluster -- not even
//! transiently in a request handler's memory. Only the resulting refresh
//! token, which is what actually gets persisted, ever crosses into the
//! cluster at all.

use std::io::Write;

use anyhow::{Context, Result};
use steam_sync::steam::{GuardType, InteractiveAuthResult, negotiate_interactive};

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run())
}

async fn run() -> Result<()> {
    let mut user = None;
    let mut password = None;
    let mut guard_code = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--user" => user = args.next(),
            "--password" => password = args.next(),
            "--guard-code" => guard_code = args.next(),
            other => anyhow::bail!("unrecognized argument '{other}' (expected --user/--password/--guard-code)"),
        }
    }

    let user = match user {
        Some(u) => u,
        None => prompt("Steam username: ")?,
    };
    let password = match password {
        Some(p) => p,
        None => rpassword::prompt_password("Steam password: ").context("failed to read password")?,
    };

    let result = negotiate_interactive(&user, &password, guard_code.as_deref())
        .await
        .context("Steam login failed")?;

    match result {
        InteractiveAuthResult::NeedsGuard { guard_type } => {
            let guard_type = match guard_type {
                GuardType::EmailCode => "email",
                GuardType::DeviceCode => "device",
            };
            println!(
                "{}",
                serde_json::json!({"needs_guard": true, "guard_type": guard_type, "steam_user": user})
            );
        }
        InteractiveAuthResult::Success { refresh_token } => {
            println!(
                "{}",
                serde_json::json!({"needs_guard": false, "steam_user": user, "refresh_token": refresh_token})
            );
        }
    }

    Ok(())
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}
