//! Generic OAuth2 login for Discord/GitHub/Google -- all three are plain
//! Authorization Code flow (no PKCE: this is a confidential server-side
//! client holding a client_secret, so PKCE's threat model doesn't apply)
//! followed by a plain authenticated GET against each provider's own "who
//! am I" REST endpoint. Deliberately not using each provider's OIDC
//! surface (Google has one, Discord/GitHub don't) -- treating all three
//! uniformly as OAuth2 + REST userinfo is simpler and avoids a third code
//! path for id_token verification that would only apply to one provider.

use oauth2::basic::BasicClient;
use oauth2::{AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet, RedirectUrl, Scope, TokenResponse, TokenUrl};

type ConfiguredClient = BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    Discord,
    Github,
    Google,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Github => "github",
            Self::Google => "google",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "discord" => Some(Self::Discord),
            "github" => Some(Self::Github),
            "google" => Some(Self::Google),
            _ => None,
        }
    }

    fn auth_url(self) -> &'static str {
        match self {
            Self::Discord => "https://discord.com/api/oauth2/authorize",
            Self::Github => "https://github.com/login/oauth/authorize",
            Self::Google => "https://accounts.google.com/o/oauth2/v2/auth",
        }
    }

    fn token_url(self) -> &'static str {
        match self {
            Self::Discord => "https://discord.com/api/oauth2/token",
            Self::Github => "https://github.com/login/oauth/access_token",
            Self::Google => "https://oauth2.googleapis.com/token",
        }
    }

    fn scopes(self) -> &'static [&'static str] {
        match self {
            Self::Discord => &["identify"],
            Self::Github => &["read:user"],
            Self::Google => &["openid", "profile"],
        }
    }

    fn userinfo_url(self) -> &'static str {
        match self {
            Self::Discord => "https://discord.com/api/users/@me",
            Self::Github => "https://api.github.com/user",
            Self::Google => "https://www.googleapis.com/oauth2/v3/userinfo",
        }
    }
}

pub struct OAuthProvider {
    pub kind: ProviderKind,
    client: ConfiguredClient,
}

impl OAuthProvider {
    pub fn new(kind: ProviderKind, client_id: String, client_secret: String, redirect_uri: String) -> anyhow::Result<Self> {
        let client = BasicClient::new(ClientId::new(client_id))
            .set_client_secret(ClientSecret::new(client_secret))
            .set_auth_uri(AuthUrl::new(kind.auth_url().to_string())?)
            .set_token_uri(TokenUrl::new(kind.token_url().to_string())?)
            .set_redirect_uri(RedirectUrl::new(redirect_uri)?);
        Ok(Self { kind, client })
    }

    /// `state` should be one of our own signed `state::StateClaims` tokens
    /// -- see that module's doc comment for why we don't use oauth2's
    /// built-in random-CSRF-token-plus-session-storage pattern.
    pub fn authorize_url(&self, state: String) -> url::Url {
        let mut req = self.client.authorize_url(move || CsrfToken::new(state));
        for scope in self.kind.scopes() {
            req = req.add_scope(Scope::new((*scope).to_string()));
        }
        let (url, _csrf) = req.url();
        url
    }

    pub async fn exchange_code(&self, http: &reqwest::Client, code: String) -> anyhow::Result<String> {
        let token = self
            .client
            .exchange_code(AuthorizationCode::new(code))
            .request_async(http)
            .await
            .map_err(|e| anyhow::anyhow!("token exchange failed: {e}"))?;
        Ok(token.access_token().secret().clone())
    }

    /// Returns `(provider_user_id, display_name)`.
    pub async fn fetch_identity(&self, http: &reqwest::Client, access_token: &str) -> anyhow::Result<(String, String)> {
        let mut req = http.get(self.kind.userinfo_url()).bearer_auth(access_token);
        if matches!(self.kind, ProviderKind::Github) {
            // GitHub's API rejects requests with no User-Agent.
            req = req.header("User-Agent", "magpie-identity");
        }
        let body: serde_json::Value = req.send().await?.error_for_status()?.json().await?;

        let (id, name) = match self.kind {
            ProviderKind::Discord => (
                body["id"].as_str().ok_or_else(|| anyhow::anyhow!("discord userinfo missing id"))?.to_string(),
                body["username"].as_str().unwrap_or_default().to_string(),
            ),
            ProviderKind::Github => (
                body["id"].as_i64().ok_or_else(|| anyhow::anyhow!("github userinfo missing id"))?.to_string(),
                body["login"].as_str().unwrap_or_default().to_string(),
            ),
            ProviderKind::Google => (
                body["sub"].as_str().ok_or_else(|| anyhow::anyhow!("google userinfo missing sub"))?.to_string(),
                body["name"].as_str().unwrap_or_default().to_string(),
            ),
        };
        Ok((id, name))
    }
}
