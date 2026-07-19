//! Provisions the technical Postgres role launcher's own Arma process
//! connects as (env vars sourced by whatever extension the running game
//! server loads -- not anything this repo owns or drives). Runs once on
//! startup, before the reconcile loop starts: reads the password out of
//! the chart-created `app_postgres_secret_name` Secret (see
//! app-postgres-secret.yaml -- generated once via Helm's own
//! `lookup`+`randAlphaNum`, stable across upgrades, so this never mints
//! its own), then connects with `database_url` (an admin-level
//! connection, distinct from the role being provisioned).
//!
//! First-creation-only, deliberately: if the role already exists, this
//! does *nothing* -- no password reset, no re-GRANT, no REVOKE. A
//! poweruser may have since granted it additional permissions on
//! purpose; unconditionally reapplying a fixed grant set on every
//! controller restart would silently claw those back. The baseline this
//! creates on first sight of the role is exactly two grants: CONNECT and
//! CREATE on the target database. CREATE is deliberately the *only*
//! privilege granted at the database level -- once the role creates its
//! own schema, it owns that schema outright (full DDL/DML rights within
//! it, standard Postgres behavior for the creating role), which is all
//! it should ever need out of the box. `REVOKE ALL ON SCHEMA public`
//! closes the one gap that would otherwise leave it: Postgres grants
//! CREATE+USAGE on `public` to every role by default (pre-15) or to
//! nobody (15+), so this makes the "no access to anything outside its
//! own schemas" guarantee explicit rather than version-dependent -- but
//! again, only as this role's one-time starting point.
//!
//! The generated password is `randAlphaNum`-only (see
//! app-postgres-secret.yaml), so it's safe to interpolate directly --
//! nothing in it can break out of a single-quoted SQL literal.

use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use kube::api::Api;
use sqlx::PgPool;

pub struct AppPostgresConfig<'a> {
    pub role: &'a str,
    pub database: &'a str,
    pub secret_name: &'a str,
}

/// Takes an already-connected `pool` (main.rs creates one long-lived
/// pool for the whole process -- reused afterward by arma_config's own
/// scope queries, not dropped after this one-time bootstrap) rather than
/// connecting itself.
pub async fn ensure_app_role(
    client: &Client,
    pool: &PgPool,
    namespace: &str,
    cfg: AppPostgresConfig<'_>,
) -> anyhow::Result<()> {
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let secret = api.get(cfg.secret_name).await?;
    let password = secret
        .data
        .as_ref()
        .and_then(|d| d.get("POSTGRES_PASSWORD"))
        .map(|b| String::from_utf8(b.0.clone()))
        .ok_or_else(|| anyhow::anyhow!("{} has no POSTGRES_PASSWORD key", cfg.secret_name))??;

    // Checked from Rust, not a server-side DO-block IF, specifically so
    // GRANT/REVOKE (not valid PL/pgSQL inside a DO block without dynamic
    // EXECUTE) can just be plain top-level statements -- and so the
    // "already existed" case can return before running any of them, per
    // the module doc.
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(cfg.role)
            .fetch_one(pool)
            .await?;
    if exists {
        tracing::info!(
            role = cfg.role,
            "app Postgres role already exists -- leaving its permissions untouched"
        );
        return Ok(());
    }

    // Role name/database come from this chart's own values (trusted, not
    // external input), so plain string interpolation is fine here --
    // Postgres has no way to bind an identifier as a query parameter
    // anyway. The password is `randAlphaNum`-only (see module doc), safe
    // inside a single-quoted literal.
    let sql = format!(
        r#"
        CREATE ROLE "{role}" LOGIN PASSWORD '{password}';
        GRANT CONNECT, CREATE ON DATABASE "{database}" TO "{role}";
        REVOKE ALL ON SCHEMA public FROM "{role}";
        "#,
        role = cfg.role,
        password = password,
        database = cfg.database,
    );
    sqlx::raw_sql(&sql).execute(pool).await?;

    tracing::info!(
        role = cfg.role,
        database = cfg.database,
        "app Postgres role created"
    );
    Ok(())
}
