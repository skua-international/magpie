use std::env;
use std::path::PathBuf;

use anyhow::Result;

/// What this process was told about Steam auth at boot, before ever
/// touching the session Secret (see `secrets.rs`) -- `main.rs` combines
/// this with whatever's actually in that Secret to decide the real
/// `SteamAuth` to start `CmPool` with, so a stored session always takes
/// priority over this when both are present. Deliberately has no
/// credentials-at-startup variant at all -- a Steam password should never
/// reach this (or any other deployed) process, not even via an env var
/// sourced from a Secret. The only ways to get a session running are
/// `Anonymous` or an already-negotiated refresh token, established
/// out-of-band via `RefreshSteamAuth` (see its own proto doc) and read
/// back from the session Secret below.
pub enum SteamAuthConfig {
    Anonymous,
    /// Anonymous login isn't configured either -- fine as long as the
    /// session Secret already has a valid refresh token (established via
    /// a prior RefreshSteamAuth call); if it doesn't either, this process
    /// starts with no Steam session at all until RefreshSteamAuth is
    /// called.
    None,
}

pub struct Config {
    pub steam_auth: SteamAuthConfig,
    /// Root of the golden, continuously-synced content tree (server depots
    /// under it directly, workshop mods under `workshop/`).
    pub content_root: PathBuf,
    /// Where per-request reflink claims get written.
    pub claims_root: PathBuf,
    pub listen_addr: String,
    pub pool_size: usize,
    /// How often the ModSource reconciler re-resolves an already-`Synced`
    /// source's candidate IDs on its own periodic requeue, independent of
    /// anything re-registering it -- catches upstream collection-membership
    /// drift even for a source nothing is actively touching right now.
    pub poll_interval_secs: u64,
    /// Namespace the ModSource reconciler watches, and where the session
    /// Secret (see `secrets.rs`) lives.
    pub namespace: String,
    /// Name of an existing Secret (pre-created by the chart, never by
    /// this process -- see secrets.rs's own doc for why) holding
    /// `steam_user`/`refresh_token` keys once a session has been
    /// established via RefreshSteamAuth.
    pub steam_session_secret_name: String,
    /// Empty if volume-manager isn't configured -- RegisterSource just
    /// skips its pre-emptive grow check in that case rather than
    /// requiring it. The chart's default values.yaml always sets this.
    pub volume_manager_url: String,
    pub volume_manager_token: String,
    pub volume_grow_grace_bytes: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let steam_auth = if bool_env("ANONYMOUS_LOGIN", false) {
            SteamAuthConfig::Anonymous
        } else {
            SteamAuthConfig::None
        };

        Ok(Self {
            steam_auth,
            content_root: env::var("CONTENT_ROOT")
                .unwrap_or_else(|_| "/content".into())
                .into(),
            claims_root: env::var("CLAIMS_ROOT")
                .unwrap_or_else(|_| "/claims".into())
                .into(),
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            pool_size: env::var("POOL_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4),
            poll_interval_secs: env::var("POLL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1800),
            namespace: env::var("NAMESPACE").unwrap_or_else(|_| "default".into()),
            steam_session_secret_name: env::var("STEAM_SESSION_SECRET_NAME")
                .unwrap_or_else(|_| "arma-steam-session".into()),
            volume_manager_url: env::var("VOLUME_MANAGER_URL").unwrap_or_default(),
            volume_manager_token: env::var("VOLUME_MANAGER_TOKEN").unwrap_or_default(),
            volume_grow_grace_bytes: env::var("VOLUME_GROW_GRACE_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10 * 1024 * 1024 * 1024), // 10GiB
        })
    }
}

fn bool_env(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(v) => v.eq_ignore_ascii_case("true"),
        Err(_) => default,
    }
}
