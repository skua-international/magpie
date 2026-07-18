use std::env;
use std::path::PathBuf;

use anyhow::Result;

/// What this process was told about Steam auth at boot, before ever
/// touching the session Secret (see `secrets.rs`) -- `main.rs` combines
/// this with whatever's actually in that Secret to decide the real
/// `SteamAuth` to start `CmPool` with, so a stored session always takes
/// priority over these when both are present.
pub enum SteamAuthConfig {
    Anonymous,
    Credentials { user: String, password: String },
    /// Neither anonymous login nor STEAM_USER/STEAM_PASSWORD were
    /// configured -- fine as long as the session Secret already has a
    /// valid refresh token (established via a prior RefreshSteamAuth
    /// call); if it doesn't either, this process starts with no Steam
    /// session at all until RefreshSteamAuth is called.
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
    /// established, either at startup from STEAM_USER/STEAM_PASSWORD or
    /// later via RefreshSteamAuth.
    pub steam_session_secret_name: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let anonymous_login = bool_env("ANONYMOUS_LOGIN", false);
        let steam_user = env::var("STEAM_USER").ok();
        let steam_password = env::var("STEAM_PASSWORD").ok();
        let steam_auth = match (anonymous_login, steam_user, steam_password) {
            (true, _, _) => SteamAuthConfig::Anonymous,
            (false, Some(user), Some(password)) => SteamAuthConfig::Credentials { user, password },
            (false, _, _) => SteamAuthConfig::None,
        };

        Ok(Self {
            steam_auth,
            content_root: env::var("CONTENT_ROOT").unwrap_or_else(|_| "/content".into()).into(),
            claims_root: env::var("CLAIMS_ROOT").unwrap_or_else(|_| "/claims".into()).into(),
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            pool_size: env::var("POOL_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(4),
            poll_interval_secs: env::var("POLL_INTERVAL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(1800),
            namespace: env::var("NAMESPACE").unwrap_or_else(|_| "default".into()),
            steam_session_secret_name: env::var("STEAM_SESSION_SECRET_NAME").unwrap_or_else(|_| "arma-steam-session".into()),
        })
    }
}

fn bool_env(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(v) => v.eq_ignore_ascii_case("true"),
        Err(_) => default,
    }
}
