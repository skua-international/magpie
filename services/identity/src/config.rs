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
    /// that's what registry/gateway's JwtVerifier checks against.
    pub issuer: String,
    pub audience: String,
    /// Origins a non-loopback OAuth `redirect_uri` is allowed to point at
    /// (see `state::issue`). Always contains this service's own
    /// `base_url` origin, which is the one a browser UI served from the
    /// same public host actually needs -- after the single-entrypoint
    /// change, identity and that UI are the same origin, so the common
    /// case needs no configuration at all. `ALLOWED_REDIRECT_ORIGINS`
    /// (comma-separated) adds any others, for a UI hosted somewhere else.
    ///
    /// Loopback is not listed here: it's allowed unconditionally, since
    /// magpiectl's callback listener binds an ephemeral port that can't
    /// be known in advance.
    pub allowed_redirect_origins: Vec<String>,
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

        // Own origin first, then any configured extras. Both go through
        // the same normalization `state::issue` compares against, so an
        // operator writing "https://ui.example.com/" (trailing slash) or
        // ":443" explicitly still matches.
        // Splitting an unset/empty var on ',' yields one empty string,
        // which the is_empty check below drops -- so an unset var adds
        // nothing rather than an invalid entry.
        let extra_origins = env::var("ALLOWED_REDIRECT_ORIGINS").unwrap_or_default();
        let mut allowed_redirect_origins = Vec::new();
        for raw in std::iter::once(base_url.as_str()).chain(extra_origins.split(',')) {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let parsed = url::Url::parse(raw).map_err(|_| {
                anyhow::anyhow!("ALLOWED_REDIRECT_ORIGINS entry {raw} is not a URL")
            })?;
            let origin = crate::state::origin_of(&parsed).ok_or_else(|| {
                anyhow::anyhow!("ALLOWED_REDIRECT_ORIGINS entry {raw} has no host to match on")
            })?;
            if !allowed_redirect_origins.contains(&origin) {
                allowed_redirect_origins.push(origin);
            }
        }

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
            allowed_redirect_origins,
            issuer: require_env("JWT_ISSUER")?,
            audience: require_env("JWT_AUDIENCE")?,
            providers,
        })
    }
}

fn require_env(key: &str) -> anyhow::Result<String> {
    env::var(key).map_err(|_| anyhow::anyhow!("missing required env var {key}"))
}
