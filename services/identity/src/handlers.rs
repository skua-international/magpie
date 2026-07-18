use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Redirect, Response};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::oauth::ProviderKind;
use crate::tokens::{TokenIssuer, TokenPair};
use crate::{state, steam};

/// A completed login's tokens, held just long enough for the one
/// server-to-server call that trades `code` for them (see `exchange`) --
/// single-use (removed from the map on either a successful exchange or
/// expiry), so this is deliberately not a general-purpose session store.
pub struct ExchangeEntry {
    tokens: TokenPair,
    expires_at: std::time::Instant,
}

const EXCHANGE_CODE_TTL: std::time::Duration = std::time::Duration::from_secs(120);

pub struct AppState {
    pub pool: sqlx::PgPool,
    pub signer: crate::signing::Signer,
    pub http: reqwest::Client,
    pub base_url: String,
    pub issuer: String,
    pub audience: String,
    pub oauth_providers: HashMap<ProviderKind, crate::oauth::OAuthProvider>,
    pub exchange_codes: Mutex<HashMap<String, ExchangeEntry>>,
}

impl AppState {
    fn token_issuer(&self) -> TokenIssuer<'_> {
        TokenIssuer { signer: &self.signer, issuer: self.issuer.clone(), audience: self.audience.clone() }
    }

    /// Verifies a bearer access token this service itself issued -- used
    /// only to authorize the "link a second provider to my account" case,
    /// so this never needs to call out to its own JWKS endpoint over HTTP.
    fn verify_bearer(&self, headers: &HeaderMap) -> Option<Uuid> {
        let token = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?;
        let key = jsonwebtoken::DecodingKey::from_jwk(&self.signer.jwk).ok()?;
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        #[derive(Deserialize)]
        struct Claims {
            sub: String,
        }
        let data = jsonwebtoken::decode::<Claims>(token, &key, &validation).ok()?;
        Uuid::parse_str(&data.claims.sub).ok()
    }
}

pub async fn healthz() -> &'static str {
    "ok"
}

pub async fn jwks(State(app): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(app.signer.jwks_json())
}

/// `GET /auth/:provider/start` -- returns `{"url": "..."}` to redirect the
/// end user to, rather than a 302 itself, so a caller that already holds
/// this user's access token (e.g. a bot linking a second provider on the
/// user's behalf) can pass it via a normal `Authorization` header on this
/// request instead of it ever having to appear inside a browser-visible
/// URL. Direct human navigation (plain login, no header) works the same
/// way, just always in login mode.
pub async fn start(
    Path(provider): Path<String>,
    State(app): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let link_user_id = app.verify_bearer(&headers);
    let redirect_uri = query.get("redirect_uri").cloned();

    let state_token = match state::issue(&app.signer, &provider, link_user_id, redirect_uri) {
        Ok(t) => t,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    };

    if provider == "steam" {
        let return_to = format!("{}/auth/steam/callback?state={}", app.base_url, urlencode(&state_token));
        let url = steam::login_url(&return_to, &app.base_url);
        return Json(json!({ "url": url.to_string() })).into_response();
    }

    let Some(kind) = ProviderKind::parse(&provider) else {
        return (StatusCode::NOT_FOUND, format!("unknown provider: {provider}")).into_response();
    };
    let Some(oauth) = app.oauth_providers.get(&kind) else {
        return (StatusCode::NOT_FOUND, format!("provider not configured: {provider}")).into_response();
    };
    Json(json!({ "url": oauth.authorize_url(state_token).to_string() })).into_response()
}

/// `GET /auth/:provider/callback` -- lands here directly from the
/// provider's own redirect. Two shapes, both driven by whether `start`
/// was called with a `redirect_uri` (see its own doc):
/// - No `redirect_uri` (a program calling this itself, e.g. a website's
///   own backend fetching the callback server-to-server): returns the
///   token pair as JSON directly, same as always.
/// - `redirect_uri` set (a real human in a browser -- the CLI's login
///   flow): 302s the browser to `redirect_uri?code=<one-time code>`
///   instead. The real tokens never appear in a URL (browser history,
///   referrer headers) -- `code` is only ever useful for one immediate
///   `POST /auth/exchange` call, which is expected to happen
///   server-to-server from whatever's listening at `redirect_uri`.
pub async fn callback(Path(provider): Path<String>, State(app): State<Arc<AppState>>, Query(query): Query<HashMap<String, String>>) -> Response {
    let Some(state_token) = query.get("state") else {
        return (StatusCode::BAD_REQUEST, "missing state").into_response();
    };
    let claims = match state::verify(&app.signer, state_token, &provider) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    let (provider_user_id, display_name) = if provider == "steam" {
        match steam::verify(&app.http, &query).await {
            Ok(steam_id) => (steam_id.clone(), steam_id),
            Err(e) => return (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
        }
    } else {
        let Some(kind) = ProviderKind::parse(&provider) else {
            return (StatusCode::NOT_FOUND, format!("unknown provider: {provider}")).into_response();
        };
        let Some(oauth) = app.oauth_providers.get(&kind) else {
            return (StatusCode::NOT_FOUND, format!("provider not configured: {provider}")).into_response();
        };
        let Some(code) = query.get("code").cloned() else {
            return (StatusCode::BAD_REQUEST, "missing code").into_response();
        };
        let access_token = match oauth.exchange_code(&app.http, code).await {
            Ok(t) => t,
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
        };
        match oauth.fetch_identity(&app.http, &access_token).await {
            Ok(identity) => identity,
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
        }
    };

    let user_id_result = match claims.link_user_id {
        Some(existing_user_id) => registry_db::link_account_to_user(&app.pool, &provider, &provider_user_id, existing_user_id, &display_name)
            .await
            .map(|()| existing_user_id),
        None => match registry_db::find_linked_account(&app.pool, &provider, &provider_user_id).await {
            Ok(Some(user_id)) => Ok(user_id),
            Ok(None) => registry_db::create_user_and_link(&app.pool, &provider, &provider_user_id, &display_name).await,
            Err(e) => Err(e),
        },
    };
    let user_id = match user_id_result {
        Ok(id) => id,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };

    let pair = match app.token_issuer().issue(&app.pool, user_id).await {
        Ok(pair) => pair,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };

    let Some(redirect_uri) = claims.redirect_uri else {
        return Json(pair).into_response();
    };
    let code = {
        use ring::rand::SecureRandom;
        let mut bytes = [0u8; 24];
        ring::rand::SystemRandom::new().fill(&mut bytes).expect("OS RNG must be available");
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
    };
    app.exchange_codes.lock().unwrap().insert(code.clone(), ExchangeEntry { tokens: pair, expires_at: std::time::Instant::now() + EXCHANGE_CODE_TTL });

    let separator = if redirect_uri.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{redirect_uri}{separator}code={}", urlencode(&code))).into_response()
}

#[derive(Deserialize)]
pub struct RefreshRequest {
    refresh_token: String,
}

pub async fn refresh(State(app): State<Arc<AppState>>, Json(body): Json<RefreshRequest>) -> Response {
    match app.token_issuer().refresh(&app.pool, &body.refresh_token).await {
        Ok(pair) => Json(pair).into_response(),
        Err(_) => (StatusCode::UNAUTHORIZED, "invalid, expired, or already-used refresh token").into_response(),
    }
}

#[derive(Deserialize)]
pub struct ExchangeRequest {
    code: String,
}

/// `POST /auth/exchange` -- trades a one-time code from a browser-redirect
/// callback (see `callback`'s own doc) for the real tokens. Whoever holds
/// `code` for the few seconds between the redirect landing and this call
/// is trusted implicitly (same threat model as an OAuth authorization
/// code) -- this is why `callback` only ever redirects to a loopback
/// address (validated at `state::issue` time): a code briefly visible in
/// a local process's own request log is a different risk than one
/// visible to any network intermediary.
pub async fn exchange(State(app): State<Arc<AppState>>, Json(body): Json<ExchangeRequest>) -> Response {
    let entry = app.exchange_codes.lock().unwrap().remove(&body.code);
    match entry {
        Some(entry) if entry.expires_at >= std::time::Instant::now() => Json(entry.tokens).into_response(),
        _ => (StatusCode::UNAUTHORIZED, "invalid, expired, or already-used exchange code").into_response(),
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
