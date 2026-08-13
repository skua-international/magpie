mod config;
mod handlers;
mod oauth;
mod signing;
mod state;
mod steam;
mod tokens;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::Router;
use axum::routing::{get, post};
use config::Config;
use handlers::AppState;
use oauth::OAuthProvider;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Logs, metrics and (when an OTLP endpoint is configured) trace
    // export, all from one place -- see crates/observability. `telemetry`
    // has to live for the rest of `main`: dropping it shuts the exporters
    // down and stops the non-blocking log writer's thread, silently
    // discarding whatever is still buffered.
    // Arc because the /metrics handler needs to be callable repeatedly
    // (axum handlers are Fn, not FnOnce) while `main` keeps its own
    // reference alive for the process's lifetime.
    let telemetry = std::sync::Arc::new(observability::init("identity")?);

    let cfg = Config::from_env()?;

    let pool = registry_db::connect(&cfg.database_url).await?;
    info!("connected to Postgres");

    let signer = signing::Signer::load_or_create(&pool).await?;
    info!("signing key ready");

    // Poll-and-set on a timer, unchanged from what this replaces -- only
    // the recording API moved to OpenTelemetry. A synchronous gauge, not
    // an observable one: an observable's callback runs on the collection
    // path, and this value comes from Postgres, so a scrape would then
    // block on a database round trip.
    let identities = observability::meter()
        .u64_gauge("magpie_identities_total")
        .with_description("Distinct identities -- one per person, not per linked provider account")
        .build();
    let metrics_pool = pool.clone();
    tokio::spawn(async move {
        loop {
            match registry_db::count_users(&metrics_pool).await {
                Ok(count) => identities.record(count.max(0) as u64, &[]),
                Err(e) => warn!("metrics poll failed: {e:#}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let mut oauth_providers = HashMap::new();
    for provider in &cfg.providers {
        let redirect_uri = format!("{}/auth/{}/callback", cfg.base_url, provider.kind.as_str());
        let client = OAuthProvider::new(
            provider.kind,
            provider.client_id.clone(),
            provider.client_secret.clone(),
            redirect_uri,
        )?;
        info!("enabled OAuth2 provider: {}", provider.kind.as_str());
        oauth_providers.insert(provider.kind, client);
    }
    if oauth_providers.is_empty() {
        info!(
            "no OAuth2 providers configured (only Steam login will work) -- set e.g. DISCORD_CLIENT_ID/DISCORD_CLIENT_SECRET to enable one"
        );
    }

    let app_state = Arc::new(AppState {
        pool,
        signer,
        http,
        base_url: cfg.base_url,
        allowed_redirect_origins: cfg.allowed_redirect_origins,
        issuer: cfg.issuer,
        audience: cfg.audience,
        oauth_providers,
        exchange_codes: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/healthz", get(handlers::healthz))
        .route(
            "/metrics",
            get({
                let telemetry = telemetry.clone();
                move || {
                    let telemetry = telemetry.clone();
                    async move { telemetry.render() }
                }
            }),
        )
        .route("/.well-known/jwks.json", get(handlers::jwks))
        .route("/auth/providers", get(handlers::providers))
        .route("/auth/me", get(handlers::me))
        .route("/auth/{provider}/start", get(handlers::start))
        .route("/auth/{provider}/callback", get(handlers::callback))
        .route("/auth/refresh", post(handlers::refresh))
        .route("/auth/exchange", post(handlers::exchange))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    info!("listening on {}", cfg.listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
