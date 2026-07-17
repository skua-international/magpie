//! The OAuth2 `state` parameter (and Steam OpenID's callback correlation)
//! *is* a short-lived JWT of our own, signed with the same key as real
//! access tokens but a distinct `aud` so one can never be mistaken for the
//! other. This is what lets login (and linking a second provider to an
//! already-authenticated user) work with zero server-side session state --
//! no cookie jar, no session store, nothing to clean up. The signature
//! plus a short expiry is the entire CSRF defense: a caller can't forge a
//! valid one without our private key, and can't replay an old one past
//! `exp`.

use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::signing::Signer;

/// Never sent to, or accepted from, anything outside this process --
/// distinct from the real access-token `aud` so a state token can't be
/// replayed as a bearer token against services/registry or
/// services/server-api even if someone tried.
const STATE_AUDIENCE: &str = "identity-state";
const STATE_TTL_SECS: i64 = 600;

#[derive(Serialize, Deserialize)]
pub struct StateClaims {
    pub aud: String,
    pub exp: i64,
    pub provider: String,
    /// `Some(user_id)` means "this is a link flow, attach the successful
    /// login to this already-existing user" rather than creating a new one.
    pub link_user_id: Option<Uuid>,
}

pub fn issue(signer: &Signer, provider: &str, link_user_id: Option<Uuid>) -> anyhow::Result<String> {
    let claims = StateClaims {
        aud: STATE_AUDIENCE.to_string(),
        exp: chrono::Utc::now().timestamp() + STATE_TTL_SECS,
        provider: provider.to_string(),
        link_user_id,
    };
    signer.sign(&claims)
}

pub fn verify(signer: &Signer, token: &str, expected_provider: &str) -> Result<StateClaims, &'static str> {
    let key = DecodingKey::from_jwk(&signer.jwk).map_err(|_| "invalid signer jwk")?;
    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_audience(&[STATE_AUDIENCE]);
    validation.validate_exp = true;

    let data = decode::<StateClaims>(token, &key, &validation).map_err(|_| "invalid or expired state")?;
    if data.claims.provider != expected_provider {
        return Err("state provider mismatch");
    }
    Ok(data.claims)
}
