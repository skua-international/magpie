use std::env;
use std::path::PathBuf;

use authn::jwt::JwtConfig;

pub struct Config {
    /// Namespace this reads/writes `ArmaServer` resources in -- must match
    /// services/controller's own NAMESPACE for the two to see the same
    /// objects.
    pub namespace: String,
    pub listen_addr: String,
    pub database_url: String,
    pub sync_daemon_url: String,
    /// Directory of built web-UI assets to serve under `/ui`, or None to
    /// serve no UI at all.
    ///
    /// Optional rather than required so this binary still runs with no UI
    /// present -- a `cargo run` during backend work, or an image built
    /// without the frontend stage. The route is only mounted when the
    /// directory actually exists, so a stale or mistyped path degrades to
    /// "no UI" instead of every request under /ui 404ing from an empty
    /// ServeDir.
    pub ui_dir: Option<PathBuf>,
    pub jwt: JwtConfig,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            namespace: env::var("NAMESPACE").unwrap_or_else(|_| "default".into()),
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8443".into()),
            database_url: require_env("DATABASE_URL")?,
            sync_daemon_url: env::var("SYNC_DAEMON_URL")
                .unwrap_or_else(|_| "http://sync-daemon:8080".into()),
            ui_dir: env::var("UI_DIR")
                .ok()
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .filter(|d| d.is_dir()),
            jwt: JwtConfig {
                jwks_url: require_env("JWKS_URL")?,
                issuer: require_env("JWT_ISSUER")?,
                audience: require_env("JWT_AUDIENCE")?,
            },
        })
    }
}

fn require_env(key: &str) -> anyhow::Result<String> {
    env::var(key).map_err(|_| anyhow::anyhow!("missing required env var {key}"))
}
