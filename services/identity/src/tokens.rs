//! Issues the (access token, refresh token) pair every successful login
//! and `/auth/refresh` call returns. Access tokens are short-lived signed
//! JWTs (see `signing.rs`) -- exactly what `authn::jwt::JwtVerifier`
//! already expects from `registry`/`gateway`. Refresh tokens are opaque
//! and stored (hashed) in Postgres so they're actually revocable, and are
//! rotated on every use: presenting one both consumes it and returns a
//! fresh one, so a stolen-but-unused refresh token has a short window
//! before rotation invalidates it under the legitimate holder anyway.

use chrono::{Duration, Utc};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::signing::Signer;

const ACCESS_TOKEN_TTL_SECS: i64 = 3600;
const REFRESH_TOKEN_TTL_DAYS: i64 = 30;

#[derive(Serialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

#[derive(Serialize, Deserialize)]
struct AccessClaims {
    sub: String,
    iss: String,
    aud: String,
    exp: i64,
    iat: i64,
}

pub struct TokenIssuer<'a> {
    pub signer: &'a Signer,
    pub issuer: String,
    pub audience: String,
}

impl<'a> TokenIssuer<'a> {
    pub async fn issue(&self, pool: &PgPool, user_id: Uuid) -> anyhow::Result<TokenPair> {
        let now = Utc::now();
        let claims = AccessClaims {
            sub: user_id.to_string(),
            iss: self.issuer.clone(),
            aud: self.audience.clone(),
            exp: (now + Duration::seconds(ACCESS_TOKEN_TTL_SECS)).timestamp(),
            iat: now.timestamp(),
        };
        let access_token = self.signer.sign(&claims)?;

        let raw_refresh = random_token();
        let hash = hash_token(&raw_refresh);
        registry_db::insert_refresh_token(
            pool,
            &hash,
            user_id,
            now + Duration::days(REFRESH_TOKEN_TTL_DAYS),
        )
        .await?;

        Ok(TokenPair {
            access_token,
            refresh_token: raw_refresh,
            expires_in: ACCESS_TOKEN_TTL_SECS,
        })
    }

    /// Consumes `raw_refresh_token` (rotation: it's revoked here
    /// regardless of what happens next) and, if it was valid, issues a
    /// fresh pair for its owning user.
    pub async fn refresh(
        &self,
        pool: &PgPool,
        raw_refresh_token: &str,
    ) -> anyhow::Result<TokenPair> {
        let hash = hash_token(raw_refresh_token);
        let user_id = registry_db::valid_refresh_token_user(pool, &hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("invalid, expired, or already-used refresh token"))?;
        registry_db::revoke_refresh_token(pool, &hash).await?;
        self.issue(pool, user_id).await
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("OS RNG must be available");
    base64_url_encode(&bytes)
}

fn hash_token(token: &str) -> String {
    use ring::digest::{SHA256, digest};
    let hash = digest(&SHA256, token.as_bytes());
    hash.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.encode(bytes)
}
