use std::env;
use std::path::PathBuf;

use anyhow::Result;
use steam_sync::steam::SteamAuth;

pub struct Config {
    pub steam_auth: SteamAuth,
    /// Root of the golden, continuously-synced content tree (server depots
    /// under it directly, workshop mods under `workshop/`).
    pub content_root: PathBuf,
    /// Where per-request reflink claims get written.
    pub claims_root: PathBuf,
    pub listen_addr: String,
    pub pool_size: usize,
    /// How often the background poller re-resolves every registered
    /// source's candidate IDs, independent of RegisterSource calls.
    pub poll_interval_secs: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let anonymous_login = bool_env("ANONYMOUS_LOGIN", false);
        let steam_auth = if anonymous_login {
            SteamAuth::Anonymous
        } else {
            SteamAuth::Credentials {
                user: require_env("STEAM_USER")?,
                password: require_env("STEAM_PASSWORD")?,
            }
        };

        Ok(Self {
            steam_auth,
            content_root: env::var("CONTENT_ROOT").unwrap_or_else(|_| "/content".into()).into(),
            claims_root: env::var("CLAIMS_ROOT").unwrap_or_else(|_| "/claims".into()).into(),
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            pool_size: env::var("POOL_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(4),
            poll_interval_secs: env::var("POLL_INTERVAL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(1800),
        })
    }
}

fn require_env(key: &str) -> Result<String> {
    env::var(key).map_err(|_| anyhow::anyhow!("missing required env var {key}"))
}

fn bool_env(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(v) => v.eq_ignore_ascii_case("true"),
        Err(_) => default,
    }
}
