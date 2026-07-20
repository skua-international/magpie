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
use metrics_exporter_prometheus::PrometheusBuilder;
use oauth::OAuthProvider;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cfg = Config::from_env()?;

    let pool = registry_db::connect(&cfg.database_url).await?;
    info!("connected to Postgres");

    let signer = signing::Signer::load_or_create(&pool).await?;
    info!("signing key ready");

    let prometheus_handle = PrometheusBuilder::new().install_recorder()?;
    let metrics_pool = pool.clone();
    tokio::spawn(async move {
        loop {
            match registry_db::count_users(&metrics_pool).await {
                Ok(count) => metrics::gauge!("magpie_identities_total").set(count as f64),
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
        issuer: cfg.issuer,
        audience: cfg.audience,
        oauth_providers,
        exchange_codes: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/healthz", get(handlers::healthz))
        .route(
            "/metrics",
            get(|| async move { prometheus_handle.render() }),
        )
        .route("/.well-known/jwks.json", get(handlers::jwks))
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
