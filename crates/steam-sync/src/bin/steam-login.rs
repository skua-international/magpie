//! A local-only operator tool: does a QR-code Steam login (no password
//! ever typed into, or handled by, this or any other process) and prints
//! the resulting refresh token as one line of JSON. Never run as a
//! deployed service -- invoked by `magpiectl admin refresh-steam-auth`
//! (cli/magpie), which owns everything downstream of the negotiation
//! itself (writing the cluster's Secret via kubectl, restarting
//! sync-daemon to pick it up). Lives as a `src/bin/` target on the
//! existing steam-sync crate rather than its own workspace member --
//! it's a thin wrapper around `begin_qr_login`/`poll_qr_login`, which
//! already live here, not enough surface to justify a whole separate
//! package.
//!
//! The point of routing this through a local process at all, rather than
//! an RPC to a deployed service: even a QR flow still needs an
//! authenticated CM session negotiated somewhere, and that should never
//! be a deployed service's job -- only the resulting refresh token, which
//! is what actually gets persisted, ever crosses into the cluster at all.

use anyhow::{Context, Result};
use steam_sync::steam::{begin_qr_login, poll_qr_login};

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run())
}

async fn run() -> Result<()> {
    let (qr, challenge_url) = begin_qr_login().await.context("failed to start Steam QR login")?;

    // s.team/q/1/... links only do anything meaningful either scanned as
    // an actual QR image by a phone camera (the Steam app intercepts it)
    // or opened directly on a device that already has the app installed
    // (app-link registration) -- opening it in a desktop browser just
    // redirects to a generic /about page, confirmed live. A real QR code
    // in the terminal is the only reliable path here, not a browser.
    print_qr(&challenge_url)?;
    eprintln!("Scan the QR code above with the Steam mobile app to confirm login.");
    eprintln!("(Or, on a device that already has the app installed: {challenge_url})");
    eprintln!("Waiting for confirmation...");

    let (username, refresh_token) = poll_qr_login(qr).await.context("Steam login failed")?;

    eprintln!("Confirmed, logged in as {username}.");
    println!(
        "{}",
        serde_json::json!({"steam_user": username, "refresh_token": refresh_token})
    );

    Ok(())
}

fn print_qr(data: &str) -> Result<()> {
    use qrcode::QrCode;
    use qrcode::render::unicode;

    let code = QrCode::new(data).context("failed to encode QR code")?;
    let image = code.render::<unicode::Dense1x2>().build();
    println!("{image}");
    Ok(())
}
