//! Shared Postgres access for the cluster's single database: mod sources,
//! missions, and the authorization grant table. Used by both
//! `services/registry` (owns this data -- ModSourceService/MissionService)
//! and `services/controller` (read-only: resolving a server's mod source
//! kind/reference when building its `-mod=` args, and checking ACL grants
//! for its own JWT-authenticated RPCs). Servers themselves are
//! deliberately *not* stored here: they're `ArmaServer` custom resources,
//! which is where the actual desired-state reconciliation this project
//! needs already lives -- duplicating that into a second, Postgres-backed
//! source of truth would just create a consistency problem with no
//! corresponding benefit.

use sqlx::PgPool;
use uuid::Uuid;

pub async fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    // rustls 0.23 (pulled in transitively by kube/reqwest, a separate
    // dependency line from sqlx's own bundled rustls 0.21) needs a
    // process-level CryptoProvider installed before its first use, or any
    // TLS handshake through it panics. Every service calls this connect()
    // on startup before touching kube::Client or reqwest, so installing it
    // here covers all of them from one place.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(10).connect(database_url).await?;
    // raw_sql, not query(): the schema block below is several
    // semicolon-separated statements in one string, which Postgres's
    // extended query protocol (what query()/prepared statements use)
    // rejects outright ("cannot insert multiple commands into a prepared
    // statement"). raw_sql uses the simple query protocol instead, which
    // supports exactly this multi-statement-string shape.
    sqlx::raw_sql(
        r#"
        CREATE TABLE IF NOT EXISTS missions (
            id UUID PRIMARY KEY,
            name TEXT NOT NULL,
            filesize BIGINT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            created_by TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS acl_grants (
            subject TEXT PRIMARY KEY,
            scopes TEXT[] NOT NULL
        );
        CREATE TABLE IF NOT EXISTS users (
            id UUID PRIMARY KEY,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE TABLE IF NOT EXISTS linked_accounts (
            provider TEXT NOT NULL,
            provider_user_id TEXT NOT NULL,
            user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            display_name TEXT NOT NULL DEFAULT '',
            linked_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (provider, provider_user_id)
        );
        -- Singleton row (id pinned to 1) so concurrent first-boot races
        -- resolve via a plain unique-constraint conflict instead of needing
        -- application-level locking.
        CREATE TABLE IF NOT EXISTS signing_keys (
            id SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
            pkcs8_der BYTEA NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        -- Opaque, revocable, rotated-on-use -- deliberately not a signed
        -- JWT like access tokens: a long-lived credential needs to be
        -- invalidatable server-side, which a self-verified JWT can't be
        -- without a separate blocklist anyway. Only the SHA-256 hash is
        -- stored, never the raw token.
        CREATE TABLE IF NOT EXISTS refresh_tokens (
            token_hash TEXT PRIMARY KEY,
            user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            expires_at TIMESTAMPTZ NOT NULL,
            revoked_at TIMESTAMPTZ
        );
        "#,
    )
    .execute(&pool)
    .await?;
    Ok(pool)
}

#[derive(sqlx::FromRow)]
pub struct MissionRow {
    pub id: Uuid,
    pub name: String,
    pub filesize: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub created_by: String,
}

pub async fn upsert_mission(pool: &PgPool, id: Uuid, name: &str, filesize: i64, created_by: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO missions (id, name, filesize, created_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, filesize = excluded.filesize",
    )
    .bind(id)
    .bind(name)
    .bind(filesize)
    .bind(created_by)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_mission(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<MissionRow>> {
    sqlx::query_as("SELECT id, name, filesize, created_at, created_by FROM missions WHERE id = $1").bind(id).fetch_optional(pool).await
}

pub async fn list_missions(pool: &PgPool) -> sqlx::Result<Vec<MissionRow>> {
    sqlx::query_as("SELECT id, name, filesize, created_at, created_by FROM missions ORDER BY created_at").fetch_all(pool).await
}

pub async fn delete_mission(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM missions WHERE id = $1").bind(id).execute(pool).await?;
    Ok(result.rows_affected() > 0)
}

/// Scopes granted to `subject` (a JWT `sub` claim value), for the
/// authorization middleware to check per-RPC required scopes against.
/// `"*"` in the returned set means "every scope" -- a coarse admin grant,
/// since there's no real role hierarchy yet.
pub async fn scopes_for_subject(pool: &PgPool, subject: &str) -> sqlx::Result<Vec<String>> {
    let row: Option<(Vec<String>,)> = sqlx::query_as("SELECT scopes FROM acl_grants WHERE subject = $1").bind(subject).fetch_optional(pool).await?;
    Ok(row.map(|(scopes,)| scopes).unwrap_or_default())
}

pub async fn grant_scopes(pool: &PgPool, subject: &str, scopes: &[String]) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO acl_grants (subject, scopes) VALUES ($1, $2)
         ON CONFLICT (subject) DO UPDATE SET scopes = excluded.scopes",
    )
    .bind(subject)
    .bind(scopes)
    .execute(pool)
    .await?;
    Ok(())
}

/// The ES256 signing keypair services/identity mints access tokens with,
/// PKCS#8 DER-encoded -- stored in Postgres rather than a Kubernetes
/// Secret specifically so the service can generate-and-persist its own key
/// on first boot with no RBAC/Secret-writing needed. Read by
/// `get_or_create_signing_key`; written directly only by that function.
pub async fn get_signing_key(pool: &PgPool) -> sqlx::Result<Option<Vec<u8>>> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT pkcs8_der FROM signing_keys WHERE id = 1").fetch_optional(pool).await?;
    Ok(row.map(|(der,)| der))
}

/// Races an insert of a freshly generated key against `signing_keys`' own
/// singleton constraint (`id` pinned to 1) -- if another replica won the
/// race first, the `INSERT` is a no-op (`ON CONFLICT DO NOTHING`) and this
/// re-reads whatever actually landed, so every caller ends up agreeing on
/// one key regardless of who generated it.
pub async fn insert_signing_key_if_absent(pool: &PgPool, pkcs8_der: &[u8]) -> sqlx::Result<Vec<u8>> {
    sqlx::query("INSERT INTO signing_keys (id, pkcs8_der) VALUES (1, $1) ON CONFLICT (id) DO NOTHING").bind(pkcs8_der).execute(pool).await?;
    let (der,): (Vec<u8>,) =
        sqlx::query_as("SELECT pkcs8_der FROM signing_keys WHERE id = 1").fetch_one(pool).await?;
    Ok(der)
}

/// A linked external identity (Discord/GitHub/Google OAuth2, or Steam
/// OpenID), if `(provider, provider_user_id)` has been seen before.
pub async fn find_linked_account(pool: &PgPool, provider: &str, provider_user_id: &str) -> sqlx::Result<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as("SELECT user_id FROM linked_accounts WHERE provider = $1 AND provider_user_id = $2")
        .bind(provider)
        .bind(provider_user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(id,)| id))
}

/// Creates a brand-new user and links this external identity to it in one
/// transaction. If this is the very first user ever created, also grants
/// it every scope (`acl_grants.scopes = ARRAY['*']`) -- there's no admin
/// UI/notification flow yet to hand out permissions any other way, so the
/// first person to ever sign in becomes the operator by convention. The
/// whole thing runs under `LOCK TABLE users` so two simultaneous first-ever
/// logins can't both win that race; this only ever matters once, at
/// bootstrap, so the heavy-handed lock is fine.
pub async fn create_user_and_link(pool: &PgPool, provider: &str, provider_user_id: &str, display_name: &str) -> sqlx::Result<Uuid> {
    let mut tx = pool.begin().await?;
    sqlx::query("LOCK TABLE users IN EXCLUSIVE MODE").execute(&mut *tx).await?;

    let is_first = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users").fetch_one(&mut *tx).await? == 0;

    let user_id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id) VALUES ($1)").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO linked_accounts (provider, provider_user_id, user_id, display_name) VALUES ($1, $2, $3, $4)")
        .bind(provider)
        .bind(provider_user_id)
        .bind(user_id)
        .bind(display_name)
        .execute(&mut *tx)
        .await?;

    if is_first {
        sqlx::query(
            "INSERT INTO acl_grants (subject, scopes) VALUES ($1, ARRAY['*'])
             ON CONFLICT (subject) DO UPDATE SET scopes = excluded.scopes",
        )
        .bind(user_id.to_string())
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(user_id)
}

/// Persists a newly issued refresh token (already hashed by the caller --
/// this crate never sees the raw secret).
pub async fn insert_refresh_token(pool: &PgPool, token_hash: &str, user_id: Uuid, expires_at: chrono::DateTime<chrono::Utc>) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO refresh_tokens (token_hash, user_id, expires_at) VALUES ($1, $2, $3)")
        .bind(token_hash)
        .bind(user_id)
        .bind(expires_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// Returns the owning user if `token_hash` is a currently valid (not
/// expired, not revoked) refresh token.
pub async fn valid_refresh_token_user(pool: &PgPool, token_hash: &str) -> sqlx::Result<Option<Uuid>> {
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT user_id FROM refresh_tokens WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()")
            .bind(token_hash)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id,)| id))
}

/// Marks a refresh token used/invalid -- called on every `/auth/refresh`
/// call against the *presented* token (rotation: each refresh token is
/// single-use, a fresh one is issued alongside the new access token) and
/// on explicit logout.
pub async fn revoke_refresh_token(pool: &PgPool, token_hash: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE refresh_tokens SET revoked_at = now() WHERE token_hash = $1 AND revoked_at IS NULL").bind(token_hash).execute(pool).await?;
    Ok(())
}

/// Links an already-known external identity to an existing, already
/// authenticated user (the `ServerService`-style "link a second provider"
/// flow) -- as opposed to `create_user_and_link`, which mints a brand-new
/// user for a first-ever login.
pub async fn link_account_to_user(pool: &PgPool, provider: &str, provider_user_id: &str, user_id: Uuid, display_name: &str) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO linked_accounts (provider, provider_user_id, user_id, display_name) VALUES ($1, $2, $3, $4)")
        .bind(provider)
        .bind(provider_user_id)
        .bind(user_id)
        .bind(display_name)
        .execute(pool)
        .await?;
    Ok(())
}
