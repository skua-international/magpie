//! JWT verification. This is the *only* auth path -- there is no
//! unauthenticated or host-local API anymore. Verifies signature/`exp`/
//! `iss`/`aud` against a configured JWKS endpoint and extracts the
//! subject; it does not decide *authorization* itself (which scopes that
//! subject has) -- that's [`crate::authz`], backed by Postgres,
//! since there's no real OIDC/OAuth2 issuer yet to carry authorization
//! claims of its own. The issuer only needs to assert identity.

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

pub struct JwtConfig {
    pub jwks_url: String,
    pub issuer: String,
    pub audience: String,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
}

pub struct JwtVerifier {
    issuer: String,
    audience: String,
    jwks: JwkSet,
}

impl JwtVerifier {
    /// Fetches the JWKS once at startup. A key rotation on the issuer's
    /// side after that requires a controller restart to pick up -- worth
    /// revisiting (periodic refresh) once a real issuer exists and rotates
    /// keys in practice.
    pub async fn fetch(cfg: &JwtConfig) -> anyhow::Result<Self> {
        let jwks = reqwest::get(&cfg.jwks_url)
            .await?
            .error_for_status()?
            .json::<JwkSet>()
            .await?;
        Ok(Self {
            issuer: cfg.issuer.clone(),
            audience: cfg.audience.clone(),
            jwks,
        })
    }

    /// Returns the verified token's subject on success.
    pub fn verify(&self, token: &str) -> Result<String, &'static str> {
        let header = decode_header(token).map_err(|_| "malformed token header")?;
        let kid = header.kid.as_deref().ok_or("token header missing kid")?;
        let jwk = self.jwks.find(kid).ok_or("unknown signing key")?;
        let key = DecodingKey::from_jwk(jwk).map_err(|_| "unsupported jwk")?;

        let alg = header.alg;
        if !matches!(alg, Algorithm::RS256 | Algorithm::ES256) {
            return Err("unsupported algorithm");
        }

        let mut validation = Validation::new(alg);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);

        let data =
            decode::<Claims>(token, &key, &validation).map_err(|_| "token verification failed")?;
        Ok(data.claims.sub)
    }
}
