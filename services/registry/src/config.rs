use std::env;

use authn::jwt::JwtConfig;

pub struct Config {
    /// Root of the shared volume holding local (zip-uploaded) mods and
    /// missions -- read-write here (this service is the sole writer),
    /// read-only in every launcher Pod the controller's reconciler creates
    /// (must be the same path there, since paths recorded here are handed
    /// back to the controller/launcher verbatim).
    pub local_content_root: String,
    pub listen_addr: String,
    pub database_url: String,
    pub sync_daemon_url: String,
    /// Namespace `ModSource` objects are created/read/deleted in.
    pub namespace: String,
    pub jwt: JwtConfig,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            local_content_root: env::var("LOCAL_CONTENT_ROOT")
                .unwrap_or_else(|_| "/local-content".into()),
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8444".into()),
            database_url: require_env("DATABASE_URL")?,
            sync_daemon_url: env::var("SYNC_DAEMON_URL")
                .unwrap_or_else(|_| "http://sync-daemon:8080".into()),
            namespace: env::var("NAMESPACE").unwrap_or_else(|_| "default".into()),
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
