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

/// rustls 0.23 (pulled in transitively by kube/reqwest, a separate
/// dependency line from sqlx's own bundled rustls 0.21) needs a
/// process-level CryptoProvider installed before its first use, or any
/// TLS handshake through it panics. [`connect`] calls this itself, so
/// services that call it before creating a `kube::Client` (the usual
/// order) get this for free -- but a service that needs a `kube::Client`
/// *before* it can call [`connect`] (e.g. to read a Secret holding
/// `database_url` itself) must call this explicitly first instead,
/// confirmed live: `kube::Client::try_default()` + its first real
/// request happens to not need TLS immediately, but the first actual API
/// call through it does, and by then it's too late.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub async fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    install_crypto_provider();

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;
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

pub async fn upsert_mission(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    filesize: i64,
    created_by: &str,
) -> sqlx::Result<()> {
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
    sqlx::query_as("SELECT id, name, filesize, created_at, created_by FROM missions WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list_missions(pool: &PgPool) -> sqlx::Result<Vec<MissionRow>> {
    sqlx::query_as(
        "SELECT id, name, filesize, created_at, created_by FROM missions ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
}

pub async fn delete_mission(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM missions WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Total distinct identities (`users` rows -- one per real person, not
/// per linked provider account: two providers linked to the same person
/// share one `users` row via `link_account_to_user`'s merge). Used for
/// services/identity's own `magpie_identities_total` metric.
pub async fn count_users(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
}

/// Scopes granted to `subject` (a JWT `sub` claim value), for the
/// authorization middleware to check per-RPC required scopes against.
/// `"*"` in the returned set means "every scope" -- a coarse admin grant,
/// since there's no real role hierarchy yet.
pub async fn scopes_for_subject(pool: &PgPool, subject: &str) -> sqlx::Result<Vec<String>> {
    let row: Option<(Vec<String>,)> =
        sqlx::query_as("SELECT scopes FROM acl_grants WHERE subject = $1")
            .bind(subject)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(scopes,)| scopes).unwrap_or_default())
}

/// SteamID64s (linked_accounts.provider_user_id, provider = "steam") of
/// every identity holding `scope` (or the coarse "*" grant) -- used by
/// services/controller's arma_config to populate main.cfg's admins[]/
/// filePatchingExceptions[] from real grants rather than a hand-maintained
/// list. `acl_grants.subject` and `linked_accounts.user_id` are the same
/// underlying user, just stored as text vs. uuid respectively (subject is
/// a JWT `sub` claim value, always a stringified user id here) -- joined
/// via a cast rather than changing either column's type.
pub async fn steam_ids_with_scope(pool: &PgPool, scope: &str) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT la.provider_user_id
         FROM acl_grants g
         JOIN linked_accounts la ON la.user_id::text = g.subject
         WHERE la.provider = 'steam'
           AND ($1 = ANY(g.scopes) OR '*' = ANY(g.scopes))",
    )
    .bind(scope)
    .fetch_all(pool)
    .await
}

/// One row per user, with their linked accounts and current scopes --
/// what `AdminService.ListAcl` serves.
///
/// LEFT JOINs from `users`, not from `acl_grants`, on purpose: someone who
/// has logged in but holds no scopes yet has no `acl_grants` row at all,
/// and they're exactly who an admin is usually looking for. Starting from
/// the grant table would list only people who already have access.
pub async fn list_acl_subjects(pool: &PgPool) -> sqlx::Result<Vec<AclSubjectRow>> {
    // Accounts are aggregated into arrays here rather than fetched in a
    // second per-user query -- one round trip regardless of user count,
    // and the ordering is fixed so the UI doesn't reshuffle between
    // refreshes. COALESCE keeps a user with no grant row (or no linked
    // account, which shouldn't happen but isn't enforced) as empty
    // arrays instead of NULLs.
    sqlx::query_as(
        "SELECT
             u.id::text AS subject,
             COALESCE(g.scopes, ARRAY[]::text[]) AS scopes,
             COALESCE(
                 array_agg(la.provider ORDER BY la.provider)
                     FILTER (WHERE la.provider IS NOT NULL),
                 ARRAY[]::text[]
             ) AS providers,
             COALESCE(
                 array_agg(la.provider_user_id ORDER BY la.provider)
                     FILTER (WHERE la.provider IS NOT NULL),
                 ARRAY[]::text[]
             ) AS provider_user_ids,
             COALESCE(
                 array_agg(la.display_name ORDER BY la.provider)
                     FILTER (WHERE la.provider IS NOT NULL),
                 ARRAY[]::text[]
             ) AS display_names
         FROM users u
         LEFT JOIN acl_grants g ON g.subject = u.id::text
         LEFT JOIN linked_accounts la ON la.user_id = u.id
         GROUP BY u.id, g.scopes
         ORDER BY u.created_at",
    )
    .fetch_all(pool)
    .await
}

/// Three parallel arrays rather than a composite type: sqlx maps Postgres
/// arrays of scalars directly, whereas an array of ROW(...) needs a
/// custom Decode impl for what is only ever zipped back together
/// immediately. They're aggregated with the same ORDER BY, so index `i`
/// is one account across all three.
#[derive(sqlx::FromRow)]
pub struct AclSubjectRow {
    pub subject: String,
    pub scopes: Vec<String>,
    pub providers: Vec<String>,
    pub provider_user_ids: Vec<String>,
    pub display_names: Vec<String>,
}

/// Replaces `subject`'s scopes, or removes the row entirely when `scopes`
/// is empty -- "no grant row" and "a grant row holding no scopes" are
/// indistinguishable to `scopes_for_subject`, and not leaving empty rows
/// behind keeps the table to actual grant holders.
pub async fn set_scopes(pool: &PgPool, subject: &str, scopes: &[String]) -> sqlx::Result<()> {
    if scopes.is_empty() {
        sqlx::query("DELETE FROM acl_grants WHERE subject = $1")
            .bind(subject)
            .execute(pool)
            .await?;
        return Ok(());
    }
    grant_scopes(pool, subject, scopes).await
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
    let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT pkcs8_der FROM signing_keys WHERE id = 1")
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(der,)| der))
}

/// Races an insert of a freshly generated key against `signing_keys`' own
/// singleton constraint (`id` pinned to 1) -- if another replica won the
/// race first, the `INSERT` is a no-op (`ON CONFLICT DO NOTHING`) and this
/// re-reads whatever actually landed, so every caller ends up agreeing on
/// one key regardless of who generated it.
pub async fn insert_signing_key_if_absent(
    pool: &PgPool,
    pkcs8_der: &[u8],
) -> sqlx::Result<Vec<u8>> {
    sqlx::query(
        "INSERT INTO signing_keys (id, pkcs8_der) VALUES (1, $1) ON CONFLICT (id) DO NOTHING",
    )
    .bind(pkcs8_der)
    .execute(pool)
    .await?;
    let (der,): (Vec<u8>,) = sqlx::query_as("SELECT pkcs8_der FROM signing_keys WHERE id = 1")
        .fetch_one(pool)
        .await?;
    Ok(der)
}

/// A linked external identity (Discord/GitHub/Google OAuth2, or Steam
/// OpenID), if `(provider, provider_user_id)` has been seen before.
/// Every provider account linked to one user, for identity's `/auth/me`.
///
/// Distinct from `find_linked_account`, which goes the other way (a
/// provider identity to its user) during login.
pub async fn linked_accounts_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> sqlx::Result<Vec<(String, String, String)>> {
    sqlx::query_as(
        "SELECT provider, provider_user_id, display_name
         FROM linked_accounts
         WHERE user_id = $1
         ORDER BY provider",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn find_linked_account(
    pool: &PgPool,
    provider: &str,
    provider_user_id: &str,
) -> sqlx::Result<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT user_id FROM linked_accounts WHERE provider = $1 AND provider_user_id = $2",
    )
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
pub async fn create_user_and_link(
    pool: &PgPool,
    provider: &str,
    provider_user_id: &str,
    display_name: &str,
) -> sqlx::Result<Uuid> {
    let mut tx = pool.begin().await?;
    sqlx::query("LOCK TABLE users IN EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;

    let is_first = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
        .fetch_one(&mut *tx)
        .await?
        == 0;

    let user_id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id) VALUES ($1)")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
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
pub async fn insert_refresh_token(
    pool: &PgPool,
    token_hash: &str,
    user_id: Uuid,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> sqlx::Result<()> {
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
pub async fn valid_refresh_token_user(
    pool: &PgPool,
    token_hash: &str,
) -> sqlx::Result<Option<Uuid>> {
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
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now() WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(token_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Links an already-known external identity to an existing, already
/// authenticated user (the `ServerService`-style "link a second provider"
/// flow) -- as opposed to `create_user_and_link`, which mints a brand-new
/// user for a first-ever login.
/// Links `provider`/`provider_user_id` to `user_id` -- the account
/// currently logged in and initiating the link (identity's `callback`
/// passes its `link_user_id` here). If that external identity was
/// already linked to a *different* user (it logged in directly via this
/// provider before ever being linked, creating its own separate account),
/// that other account is merged into `user_id` rather than the link
/// simply failing on the (provider, provider_user_id) primary key: every
/// one of its own linked_accounts rows (not just this one -- if it was
/// itself a hub of other linked providers, all of them move too) is
/// re-pointed onto `user_id`, its acl_grants scopes are unioned into
/// `user_id`'s (so e.g. a stray identity that happened to be the
/// first-ever superuser doesn't silently lose that scope), and the
/// now-empty other account is deleted. All under one transaction so a
/// concurrent login can't observe a half-merged state.
pub async fn link_account_to_user(
    pool: &PgPool,
    provider: &str,
    provider_user_id: &str,
    user_id: Uuid,
    display_name: &str,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;

    let other_user_id: Option<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM linked_accounts WHERE provider = $1 AND provider_user_id = $2 FOR UPDATE")
            .bind(provider)
            .bind(provider_user_id)
            .fetch_optional(&mut *tx)
            .await?;

    if let Some(other_user_id) = other_user_id {
        if other_user_id != user_id {
            // Union other_user_id's scopes into user_id's -- ON CONFLICT
            // handles user_id not having a row yet (a fresh account with
            // no explicit grants of its own) the same as it already
            // having one.
            sqlx::query(
                "INSERT INTO acl_grants (subject, scopes)
                 SELECT $2::text, scopes FROM acl_grants WHERE subject = $1::text
                 ON CONFLICT (subject) DO UPDATE SET scopes = (
                     SELECT ARRAY(SELECT DISTINCT unnest(acl_grants.scopes || excluded.scopes))
                 )",
            )
            .bind(other_user_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM acl_grants WHERE subject = $1::text")
                .bind(other_user_id)
                .execute(&mut *tx)
                .await?;

            // Re-point every linked_accounts row other_user_id had (this
            // one included) onto user_id -- covers the case where the
            // stray account itself had more than one provider linked.
            sqlx::query("UPDATE linked_accounts SET user_id = $1 WHERE user_id = $2")
                .bind(user_id)
                .bind(other_user_id)
                .execute(&mut *tx)
                .await?;

            // refresh_tokens for other_user_id are now pointless (nobody
            // will ever present a token minted for a user_id that no
            // longer exists) -- ON DELETE CASCADE on the users row below
            // would clean these up anyway, but being explicit here keeps
            // this transaction's intent readable end to end.
            sqlx::query("DELETE FROM refresh_tokens WHERE user_id = $1")
                .bind(other_user_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM users WHERE id = $1")
                .bind(other_user_id)
                .execute(&mut *tx)
                .await?;
        }
    } else {
        sqlx::query("INSERT INTO linked_accounts (provider, provider_user_id, user_id, display_name) VALUES ($1, $2, $3, $4)")
            .bind(provider)
            .bind(provider_user_id)
            .bind(user_id)
            .bind(display_name)
            .execute(&mut *tx)
            .await?;
    }

    // Whichever branch ran, the (provider, provider_user_id) row now
    // exists and belongs to user_id -- refresh its display_name to
    // whatever this login just fetched.
    sqlx::query("UPDATE linked_accounts SET display_name = $1 WHERE provider = $2 AND provider_user_id = $3")
        .bind(display_name)
        .bind(provider)
        .bind(provider_user_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}
