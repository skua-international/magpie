use std::env;

pub struct Config {
    pub listen_addr: String,
    pub database_url: String,
    /// This service's own externally-reachable base URL (no trailing
    /// slash), e.g. `https://id.example.com` -- used to build OAuth2
    /// redirect_uri values and Steam's OpenID realm/return_to.
    pub base_url: String,
    /// `iss`/`aud` minted into every access token -- must match
    /// `jwt.issuer`/`jwt.audience` in the Helm chart's values.yaml, since
    /// that's what registry/server-api's JwtVerifier checks against.
    pub issuer: String,
    pub audience: String,
    pub providers: Vec<ProviderConfig>,
}

pub struct ProviderConfig {
    pub kind: crate::oauth::ProviderKind,
    pub client_id: String,
    pub client_secret: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let base_url = require_env("BASE_URL")?.trim_end_matches('/').to_string();

        let mut providers = Vec::new();
        for (kind, prefix) in [
            (crate::oauth::ProviderKind::Discord, "DISCORD"),
            (crate::oauth::ProviderKind::Github, "GITHUB"),
            (crate::oauth::ProviderKind::Google, "GOOGLE"),
        ] {
            let id_key = format!("{prefix}_CLIENT_ID");
            let secret_key = format!("{prefix}_CLIENT_SECRET");
            if let (Ok(client_id), Ok(client_secret)) = (env::var(&id_key), env::var(&secret_key)) {
                providers.push(ProviderConfig {
                    kind,
                    client_id,
                    client_secret,
                });
            }
        }

        Ok(Self {
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8445".into()),
            database_url: require_env("DATABASE_URL")?,
            base_url,
            issuer: require_env("JWT_ISSUER")?,
            audience: require_env("JWT_AUDIENCE")?,
            providers,
        })
    }
}

fn require_env(key: &str) -> anyhow::Result<String> {
    env::var(key).map_err(|_| anyhow::anyhow!("missing required env var {key}"))
}
