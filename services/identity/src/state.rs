//! The OAuth2 `state` parameter (and Steam OpenID's callback correlation)
//! *is* a short-lived JWT of our own, signed with the same key as real
//! access tokens but a distinct `aud` so one can never be mistaken for the
//! other. This is what lets login (and linking a second provider to an
//! already-authenticated user) work with zero server-side session state --
//! no cookie jar, no session store, nothing to clean up. The signature
//! plus a short expiry is the entire CSRF defense: a caller can't forge a
//! valid one without our private key, and can't replay an old one past
//! `exp`.

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::signing::Signer;

/// Never sent to, or accepted from, anything outside this process --
/// distinct from the real access-token `aud` so a state token can't be
/// replayed as a bearer token against services/registry or
/// services/gateway even if someone tried.
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
    /// Set when the caller is a human in a real browser rather than a
    /// program making the request itself (a website's own backend,
    /// fetching the callback server-to-server) -- on success, the
    /// callback redirects here with a one-time exchange code instead of
    /// returning the real tokens as JSON directly (see handlers.rs's
    /// `exchange` endpoint for why: a token in a redirect URL is visible
    /// in browser history/referrer headers, an exchange code that's only
    /// ever useful for one immediate server-to-server call isn't).
    /// Validated to a loopback host at issue time -- see `issue`'s own
    /// doc -- so this can't be turned into an open redirect.
    pub redirect_uri: Option<String>,
}

/// Serializes a URL's origin the way `issue` compares them: scheme, host
/// and port, with the default port for the scheme omitted. Comparing
/// origins rather than raw strings is what makes `https://ui.example.com`
/// and `https://ui.example.com:443/some/path` agree, and what keeps a
/// path from ever widening the match.
///
/// Returns None for anything without a tuple origin (a `data:` URL, say),
/// which then simply fails the allowlist check.
pub fn origin_of(url: &url::Url) -> Option<String> {
    let origin = url.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// The open-redirect guard: a `redirect_uri` is acceptable only if it's
/// loopback or its origin was explicitly configured.
///
/// Loopback is unconditional -- that's magpiectl's local callback
/// listener, which binds an ephemeral port, so no allowlist could name it
/// ahead of time. Everything else must match a configured origin exactly.
/// That second arm is what lets a browser-served UI on a real hostname
/// log in at all: the loopback-only rule this replaces rejected every
/// non-loopback redirect with a 400, which no web UI can work around. It
/// is still a guard, not an opening -- an origin has to have been named
/// in config to be accepted, so a caller can't steer the callback at a
/// host of its own choosing.
pub fn check_redirect_uri(uri: &str, allowed_origins: &[String]) -> anyhow::Result<()> {
    let parsed =
        url::Url::parse(uri).map_err(|_| anyhow::anyhow!("redirect_uri is not a valid URL"))?;
    // Matched on the parsed host rather than host_str(): for an IPv6
    // literal that returns the bracketed form ("[::1]"), so the string
    // comparison against "::1" this replaces could never match and IPv6
    // loopback was silently rejected. Typed matching also covers the
    // whole 127.0.0.0/8 range instead of the single literal 127.0.0.1 --
    // all of it is loopback, none of it is reachable off-host.
    let is_loopback = match parsed.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if is_loopback {
        return Ok(());
    }
    let matched = origin_of(&parsed).is_some_and(|origin| allowed_origins.contains(&origin));
    if !matched {
        anyhow::bail!(
            "redirect_uri must be a loopback address (127.0.0.1/localhost) or a \
             configured allowed origin -- got {uri}"
        );
    }
    Ok(())
}

pub fn issue(
    signer: &Signer,
    provider: &str,
    link_user_id: Option<Uuid>,
    redirect_uri: Option<String>,
    allowed_origins: &[String],
) -> anyhow::Result<String> {
    if let Some(uri) = &redirect_uri {
        check_redirect_uri(uri, allowed_origins)?;
    }
    let claims = StateClaims {
        aud: STATE_AUDIENCE.to_string(),
        exp: chrono::Utc::now().timestamp() + STATE_TTL_SECS,
        provider: provider.to_string(),
        link_user_id,
        redirect_uri,
    };
    signer.sign(&claims)
}

pub fn verify(
    signer: &Signer,
    token: &str,
    expected_provider: &str,
) -> Result<StateClaims, &'static str> {
    let key = DecodingKey::from_jwk(&signer.jwk).map_err(|_| "invalid signer jwk")?;
    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_audience(&[STATE_AUDIENCE]);
    validation.validate_exp = true;

    let data =
        decode::<StateClaims>(token, &key, &validation).map_err(|_| "invalid or expired state")?;
    if data.claims.provider != expected_provider {
        return Err("state provider mismatch");
    }
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::check_redirect_uri;

    fn allowed() -> Vec<String> {
        vec!["https://api.example.com".to_string()]
    }

    #[test]
    fn loopback_allowed_without_configuration() {
        // magpiectl's listener: arbitrary ephemeral port, nothing
        // configured, must still work.
        for uri in [
            "http://127.0.0.1:53017/callback",
            "http://localhost:8080/callback",
            "http://[::1]:9000/callback",
        ] {
            assert!(
                check_redirect_uri(uri, &[]).is_ok(),
                "{uri} should be allowed"
            );
        }
    }

    #[test]
    fn configured_origin_allowed_regardless_of_path_or_default_port() {
        for uri in [
            "https://api.example.com/ui/auth/callback",
            "https://api.example.com:443/ui/auth/callback",
            "https://api.example.com",
        ] {
            assert!(
                check_redirect_uri(uri, &allowed()).is_ok(),
                "{uri} should match the configured origin"
            );
        }
    }

    #[test]
    fn unconfigured_origin_rejected() {
        // The open-redirect case the guard exists for.
        for uri in [
            "https://evil.example.com/callback",
            // Subdomain of an allowed origin is still a different origin.
            "https://evil.api.example.com/callback",
            // Scheme and port are part of the origin.
            "http://api.example.com/callback",
            "https://api.example.com:8443/callback",
        ] {
            assert!(
                check_redirect_uri(uri, &allowed()).is_err(),
                "{uri} should be rejected"
            );
        }
    }

    #[test]
    fn hostname_containing_loopback_string_is_not_loopback() {
        // Guards the matches! against ever becoming a substring check.
        for uri in [
            "https://localhost.evil.com/callback",
            "https://127.0.0.1.evil.com/callback",
        ] {
            assert!(
                check_redirect_uri(uri, &allowed()).is_err(),
                "{uri} must not be treated as loopback"
            );
        }
    }

    #[test]
    fn non_tuple_origin_rejected() {
        // No host to compare, so it can never match an allowlist entry.
        assert!(check_redirect_uri("data:text/html,<script>", &allowed()).is_err());
    }

    #[test]
    fn garbage_rejected() {
        assert!(check_redirect_uri("not a url", &allowed()).is_err());
    }
}
