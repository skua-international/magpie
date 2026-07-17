//! Steam "Sign In" is OpenID 2.0 -- Valve never adopted OIDC. This is a
//! small, direct implementation of the two requests the spec-minimal flow
//! actually needs: build the redirect to Steam's OpenID provider, then
//! verify the signed callback by asking Steam itself to confirm it
//! (`openid.mode=check_authentication`) rather than trying to verify the
//! signature locally.

use std::collections::HashMap;

use url::Url;

const STEAM_OPENID_URL: &str = "https://steamcommunity.com/openid/login";

/// `return_to` is our own callback URL (already carrying our signed state
/// token as a query param -- Steam round-trips unknown query params on
/// `return_to` back to us untouched). `realm` is the same origin, without
/// a path, as OpenID's trust-root parameter.
pub fn login_url(return_to: &str, realm: &str) -> Url {
    let mut url = Url::parse(STEAM_OPENID_URL).expect("static URL is valid");
    url.query_pairs_mut()
        .append_pair("openid.ns", "http://specs.openid.net/auth/2.0")
        .append_pair("openid.mode", "checkid_setup")
        .append_pair("openid.return_to", return_to)
        .append_pair("openid.realm", realm)
        .append_pair("openid.identity", "http://specs.openid.net/auth/2.0/identifier_select")
        .append_pair("openid.claimed_id", "http://specs.openid.net/auth/2.0/identifier_select");
    url
}

/// Verifies a callback's query parameters against Steam and, on success,
/// returns the SteamID64 extracted from `openid.claimed_id`
/// (`https://steamcommunity.com/openid/id/<steamid64>`).
pub async fn verify(http: &reqwest::Client, query: &HashMap<String, String>) -> anyhow::Result<String> {
    let mut params: Vec<(String, String)> = query.iter().filter(|(k, _)| k.starts_with("openid.")).map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in params.iter_mut() {
        if k == "openid.mode" {
            *v = "check_authentication".to_string();
        }
    }

    let body = http.post(STEAM_OPENID_URL).form(&params).send().await?.text().await?;
    if !body.lines().any(|l| l.trim() == "is_valid:true") {
        anyhow::bail!("steam openid verification failed");
    }

    let claimed_id = query.get("openid.claimed_id").ok_or_else(|| anyhow::anyhow!("callback missing openid.claimed_id"))?;
    let steam_id = claimed_id.rsplit('/').next().filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("malformed claimed_id"))?;
    if steam_id.is_empty() || !steam_id.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("claimed_id did not end in a numeric SteamID64: {claimed_id}");
    }
    Ok(steam_id.to_string())
}
