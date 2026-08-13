use std::env;

use authn::jwt::JwtConfig;

pub struct Config {
    /// Namespace this reads/writes `ArmaServer` resources in -- must match
    /// services/controller's own NAMESPACE for the two to see the same
    /// objects.
    pub namespace: String,
    pub listen_addr: String,
    pub database_url: String,
    pub sync_daemon_url: String,
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
