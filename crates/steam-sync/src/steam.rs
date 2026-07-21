use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use futures::stream::{FuturesUnordered, StreamExt};
use steamdepot::auth;
use steamdepot::cdn::CdnPool;
use steamdepot::connection::CmConnection;
use steamdepot::depot::{self, DepotPlan, DownloadPlan};
use steamdepot::download::{self, decrypt_manifest_filenames};
use steamdepot::error::Error as SteamError;
use steamdepot::login;
pub use steamdepot::login::GuardType;
use steamdepot::pics;
use steamdepot::steam_api::cm_list::{self, CmServerType};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::cache;

/// Steam login mode: either an anonymous session (works for the base
/// server + Linux binary depots, but not CDLC or workshop content, since
/// Steam won't grant free licenses or item access to anonymous sessions),
/// or a real account.
#[derive(Clone)]
pub enum SteamAuth {
    Anonymous,
    Credentials {
        user: String,
        password: String,
    },
    /// Reuse an already-negotiated refresh token directly -- no RSA/
    /// begin-auth-session/poll negotiation, no password needed at all.
    /// Used when a session was established out-of-band (e.g. an
    /// interactive credential+guard-code exchange run once, elsewhere)
    /// and persisted somewhere this process reads at startup, rather than
    /// negotiated fresh here.
    Session {
        user: String,
        refresh_token: String,
    },
}

const ARMA3_SERVER_APP_ID: u32 = 233780;
const TARGET_OS: &str = "linux";
const TARGET_BRANCH: &str = "creatordlc";

/// depot IDs for each free Creator DLC, as they appear on app 233780's
/// `creatordlc` branch.
fn cdlc_depot_id(name: &str) -> Option<u32> {
    Some(match name {
        "csla" => 233793,
        "gm" => 233792,
        "vn" => 233794,
        "ws" => 233795,
        "spe" => 233788,
        "rf" => 233799,
        "ef" => 233798,
        _ => return None,
    })
}

/// Resolve CDLC names to their depot IDs, warning on (and dropping) any
/// name that isn't recognized instead of failing the whole resolution over
/// it.
pub fn cdlc_depot_ids(names: &[String]) -> Vec<u32> {
    names
        .iter()
        .filter_map(|name| match cdlc_depot_id(&name.to_lowercase()) {
            Some(id) => Some(id),
            None => {
                warn!("unknown CDLC '{name}', skipping");
                None
            }
        })
        .collect()
}
// Steam's CDN can take a lot more than the old conservative defaults --
// bumped from 8/6 to see where actual rate-limiting or bandwidth ceilings
// show up. Watch for 429/503s in the logs (retried automatically, but a
// rising rate means we've found the real limit) if pushing further.
const DOWNLOAD_WORKERS: usize = 24;

/// Global cap on how many depots/mods sync concurrently (server depots and
/// workshop mods share this one budget, since their chunk downloads now run
/// in the same pool -- see [`resolve_and_spawn_server`] /
/// [`resolve_and_spawn_workshop`]). With a large preset (100+ mods) sharing
/// this budget, too low a cap serializes big items behind each other rather
/// than actually bounding resource usage -- bumped from 10.
pub const SYNC_CONCURRENCY: usize = 128;

/// Timeout for any single CM-bound call (key fetch, manifest request code,
/// product info, ...). A hang here should surface as a specific, labeled
/// error instead of the whole process silently going quiet.
const CM_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn with_timeout<T, E>(
    label: impl std::fmt::Display,
    fut: impl Future<Output = std::result::Result<T, E>>,
) -> Result<T>
where
    anyhow::Error: From<E>,
{
    match tokio::time::timeout(CM_CALL_TIMEOUT, fut).await {
        Ok(result) => result.map_err(Into::into),
        Err(_) => bail!("timed out after {CM_CALL_TIMEOUT:?} waiting on: {label}"),
    }
}

/// A depot/mod's full sync, already bound to install dir + CDN client --
/// what actually gets pushed into the shared download task pool.
pub type SyncTasks = JoinSet<Result<()>>;

/// Spawn `fut` into `tasks` immediately, deferring the semaphore permit
/// acquisition to *inside* the spawned task. Used so server depots and
/// workshop mods can share one global concurrency limit instead of each
/// having their own separate, uncoordinated bound.
///
/// Acquiring the permit before spawning (as this used to do) makes the
/// *caller* -- `resolve_and_spawn_server`/`workshop::preset`'s resolution
/// loop -- block on an earlier item's chunk verification/download finishing
/// once all permits are in use. With more items than permits (e.g. a
/// 100+-mod preset against `SYNC_CONCURRENCY`), that stalls resolution
/// itself on download completion, showing up as misleadingly slow
/// "resolution" time when the queueing is actually download work. Spawning
/// first and acquiring the permit inside the task keeps resolution's own
/// timing honest -- queueing time now correctly falls under whatever
/// awaits the spawned tasks (`tasks.join_next()`), not resolution.
pub fn spawn_bounded(
    tasks: &Mutex<SyncTasks>,
    sem: Arc<Semaphore>,
    fut: impl Future<Output = Result<()>> + Send + 'static,
) {
    tasks.lock().unwrap().spawn(async move {
        let _permit = sem.acquire_owned().await.expect("semaphore never closed");
        fut.await
    });
}

/// A fixed-size pool of authenticated CM connections, all logged in
/// concurrently at startup, so independent resolution work (server depots,
/// workshop mods) can each get their own connection and actually run at the
/// same time instead of serializing on one shared connection. Still one
/// Steam account -- multiple connections logged into the same account is
/// fine for non-interactive/service-style use (the interactive client's
/// "kick other sessions" behavior doesn't apply to CM-level logons like
/// this).
pub struct CmPool {
    sem: Arc<Semaphore>,
    idle: Mutex<Vec<(usize, CmConnection)>>,
    /// How to log a single slot back in if it's marked bad and dropped
    /// (see [`PooledConn::mark_bad`]) -- `None` for anonymous auth (a
    /// fresh anonymous `ClientLogon` needs no shared state);
    /// `Some((username, refresh_token))` otherwise, reusing whatever
    /// token this pool already logged in with, never a password again.
    relogin: Option<(String, String)>,
}

impl CmPool {
    /// Log in `n` connections concurrently right away. `install_dir` is
    /// only used to cache/reuse a credentialed login's refresh token across
    /// process restarts (see [`negotiate_or_reuse_refresh_token`]) -- has no
    /// effect for anonymous or already-tokened auth.
    pub async fn start(n: usize, auth: &SteamAuth, install_dir: &Path) -> Result<Arc<Self>> {
        let (conns, relogin): (Vec<(usize, CmConnection)>, Option<(String, String)>) = match auth {
            SteamAuth::Anonymous => {
                // Anonymous login is already just one lightweight ClientLogon
                // per connection -- nothing to negotiate once and reuse.
                let logins = (0..n).map(|slot| async move {
                    let conn = login_anonymous().await?;
                    debug!("pool slot {slot} logged in");
                    Ok::<_, anyhow::Error>((slot, conn))
                });
                let conns = futures::future::try_join_all(logins)
                    .await
                    .context("failed to start connection pool")?;
                (conns, None)
            }
            SteamAuth::Credentials { user, password } => {
                // RSA-key/begin-auth-session/poll only needs to happen once
                // per account -- the resulting refresh token logs on any
                // number of connections via the much cheaper
                // login_with_refresh_token (a single ClientLogon), instead
                // of every pool slot repeating the whole negotiation dance.
                // negotiate_or_reuse_refresh_token additionally persists
                // that token to disk and tries it first on the *next*
                // process start, so most restarts skip the negotiation
                // entirely -- same ClientLogon-only cost as anonymous login.
                let refresh_token =
                    negotiate_or_reuse_refresh_token(user, password, install_dir).await?;
                let logins = (0..n).map(|slot| {
                    let refresh_token = refresh_token.clone();
                    async move {
                        let conn = login_with_refresh_token(user, &refresh_token).await?;
                        debug!("pool slot {slot} logged in");
                        Ok::<_, anyhow::Error>((slot, conn))
                    }
                });
                let conns = futures::future::try_join_all(logins)
                    .await
                    .context("failed to start connection pool")?;
                (conns, Some((user.clone(), refresh_token)))
            }
            SteamAuth::Session {
                user,
                refresh_token,
            } => {
                // No negotiation, no disk cache -- the token was already
                // established elsewhere (an interactive RefreshSteamAuth
                // call) and handed to us directly.
                let logins = (0..n).map(|slot| {
                    let refresh_token = refresh_token.clone();
                    async move {
                        let conn = login_with_refresh_token(user, &refresh_token).await?;
                        debug!("pool slot {slot} logged in");
                        Ok::<_, anyhow::Error>((slot, conn))
                    }
                });
                let conns = futures::future::try_join_all(logins)
                    .await
                    .context("failed to start connection pool")?;
                (conns, Some((user.clone(), refresh_token.clone())))
            }
        };

        Ok(Arc::new(Self {
            sem: Arc::new(Semaphore::new(n)),
            idle: Mutex::new(conns),
            relogin,
        }))
    }

    /// Re-log-in a single slot after [`PooledConn::mark_bad`] discarded its
    /// connection, and push the fresh one back into the idle pool. Holds
    /// `permit` for the whole attempt (including retries) so the
    /// semaphore's live-permit count never exceeds `idle`'s real length --
    /// dropping it only once either a replacement connection is in `idle`
    /// or every attempt has failed, in which case the pool is left
    /// permanently one slot short until the process restarts (a clear,
    /// logged, rare-in-practice degradation rather than a silent one).
    async fn relogin_slot(self: Arc<Self>, slot: usize, permit: tokio::sync::OwnedSemaphorePermit) {
        const RELOGIN_ATTEMPTS: usize = 3;
        for attempt in 1..=RELOGIN_ATTEMPTS {
            let result = match &self.relogin {
                Some((user, token)) => login_with_refresh_token(user, token).await,
                None => login_anonymous().await,
            };
            match result {
                Ok(conn) => {
                    info!(
                        "pool slot {slot} reconnected after being marked bad (attempt {attempt}/{RELOGIN_ATTEMPTS})"
                    );
                    self.idle.lock().unwrap().push((slot, conn));
                    return;
                }
                Err(e) if attempt < RELOGIN_ATTEMPTS => {
                    warn!(
                        "pool slot {slot} reconnect attempt {attempt}/{RELOGIN_ATTEMPTS} failed: {e:#}"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
                }
                Err(e) => {
                    error!(
                        "pool slot {slot} failed to reconnect after {RELOGIN_ATTEMPTS} attempts, giving up -- pool is now permanently short by one slot until process restart: {e:#}"
                    );
                }
            }
        }
        drop(permit);
    }

    /// The (username, refresh_token) this pool is currently logged in
    /// with, if it's credentialed (`None` for anonymous auth) -- lets a
    /// caller that just called [`CmPool::start`] persist whatever token
    /// was used/negotiated, without needing to separately track it itself.
    pub fn session(&self) -> Option<(&str, &str)> {
        self.relogin
            .as_ref()
            .map(|(user, token)| (user.as_str(), token.as_str()))
    }

    /// Check out a connection, waiting if all `n` are currently in use.
    pub async fn acquire(self: &Arc<Self>) -> PooledConn {
        let permit = self
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed");
        let (slot, conn) = self
            .idle
            .lock()
            .unwrap()
            .pop()
            .expect("permit acquired implies a connection is idle");
        debug!("pool slot {slot} acquired");
        PooledConn {
            slot,
            conn: Some(conn),
            pool: self.clone(),
            bad: false,
            permit: Some(permit),
        }
    }

    /// Shut down every pooled connection. Call once all work using the
    /// pool has finished.
    pub async fn shutdown(&self) {
        let conns: Vec<(usize, CmConnection)> = std::mem::take(&mut self.idle.lock().unwrap());
        for (slot, mut conn) in conns {
            debug!("pool slot {slot} shutting down");
            let _ = conn.shutdown().await;
        }
    }
}

/// RAII guard: derefs to the underlying `CmConnection`, returns it to the
/// pool on drop for reuse instead of logging in again. `slot` identifies
/// which of the pool's connections this is, for tracing which one a given
/// CM call is actually running on.
pub struct PooledConn {
    pub slot: usize,
    conn: Option<CmConnection>,
    pool: Arc<CmPool>,
    bad: bool,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl PooledConn {
    /// Mark this connection as unhealthy. On drop, instead of returning it
    /// to the idle pool as if nothing happened, the pool discards it and
    /// spawns a background re-login to replace the slot (see
    /// [`CmPool::relogin_slot`]). Call this after an operation on this
    /// connection fails with a transient/connection-level error (see
    /// `is_transient`) -- `CmPool` has no active health-checking
    /// otherwise (the heartbeat task just silently stops on a failed
    /// send, see its own doc), so a connection that's genuinely dead
    /// would otherwise sit in the idle pool forever, poisoning every
    /// future `acquire()` that draws it.
    pub fn mark_bad(&mut self) {
        self.bad = true;
    }
}

impl std::ops::Deref for PooledConn {
    type Target = CmConnection;
    fn deref(&self) -> &CmConnection {
        self.conn.as_ref().expect("connection taken")
    }
}

impl std::ops::DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut CmConnection {
        self.conn.as_mut().expect("connection taken")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        let (Some(conn), Some(permit)) = (self.conn.take(), self.permit.take()) else {
            return;
        };
        if self.bad {
            debug!(
                "pool slot {} marked bad, reconnecting in background",
                self.slot
            );
            let pool = self.pool.clone();
            let slot = self.slot;
            tokio::spawn(async move {
                // Best-effort: a connection we already know is bad may
                // fail to shut down cleanly too -- discarding it either
                // way, so a shutdown error here isn't worth surfacing.
                let mut conn = conn;
                let _ = conn.shutdown().await;
                pool.relogin_slot(slot, permit).await;
            });
        } else {
            debug!("pool slot {} released", self.slot);
            self.pool.idle.lock().unwrap().push((self.slot, conn));
            drop(permit);
        }
    }
}

/// Connection-level or clearly transient Steam-side conditions -- retrying
/// the same attempt is the right response, not treating it as a genuine
/// rejection. Deliberately an allowlist: anything not explicitly
/// known-transient is treated as real, since wrongly calling a real failure
/// "transient" risks retrying something that can never succeed, while
/// wrongly calling a transient blip "real" just costs one unnecessary
/// fallback (e.g. a renegotiation) -- the safer direction to be wrong in.
pub fn is_transient(e: &anyhow::Error) -> bool {
    let Some(steam_err) = e.chain().find_map(|c| c.downcast_ref::<SteamError>()) else {
        // Didn't even get far enough to produce a typed Steam error at all
        // (e.g. the timeout wrapper in with_timeout) -- treat as transient,
        // same reasoning as the connection-level variants below.
        return true;
    };
    match steam_err {
        SteamError::WebSocket(_)
        | SteamError::Io(_)
        | SteamError::ConnectionClosed
        | SteamError::ServiceMethodTimeout(_) => true,
        SteamError::EResult { eresult, .. } => matches!(
            eresult,
            3   // NoConnection
                | 10 // Busy
                | 16 // Timeout
                | 33 // ConnectFailed
                | 34 // HandshakeFailed
                | 35 // IOFailure
                | 36 // RemoteDisconnect
                | 50 // RateLimitExceeded
        ),
        _ => false,
    }
}

const DEFAULT_RETRY_ATTEMPTS: usize = 3;

/// Retry `f` up to `attempts` times total, but only on a transient failure
/// (see [`is_transient`]) -- a non-transient error returns immediately
/// without burning the remaining attempts, since retrying a genuine
/// rejection can't help. Short linear backoff between attempts (500ms *
/// attempt number) rather than a fixed delay, so a real outage doesn't get
/// hammered with retries at the same cadence a brief blip would tolerate.
///
/// Takes a real `AsyncFnMut` (not `FnMut() -> impl Future`) specifically so
/// callers can close over a `&mut CmConnection` and reborrow it fresh on
/// each retry -- a plain `FnMut() -> Fut` closure can't express "the
/// returned future's borrow doesn't outlive this one call" without extra
/// lifetime machinery, which async closures handle natively.
/// `f` takes the resource (almost always a `&mut CmConnection`) as an
/// argument rather than capturing it, and is bounded `for<'r> FnMut(&'r mut
/// R) -> ... + 'r` -- this is what actually makes a fresh reborrow per
/// retry attempt sound. A closure that instead *captures* `&mut R` from an
/// enclosing scope and reborrows it internally (`let r = &mut *r; ...`)
/// can't work here: the reborrow is a local created inside the closure
/// body, and the returned future borrowing it would be a reference
/// escaping that local's scope once the closure call returns -- the same
/// rule that stops a plain function from returning a reference to its own
/// local variable. Taking the resource as a genuine argument ties the
/// future's borrow to the *caller's* value (this loop, which lives across
/// the `.await`), not to anything created inside `f` itself.
async fn with_retry<T, R>(
    label: impl std::fmt::Display,
    attempts: usize,
    resource: &mut R,
    mut f: impl for<'r> FnMut(
        &'r mut R,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<T>> + Send + 'r>>,
) -> Result<T> {
    for attempt in 1..=attempts {
        match f(resource).await {
            Ok(v) => return Ok(v),
            Err(e) if is_transient(&e) && attempt < attempts => {
                warn!("transient error on {label} (attempt {attempt}/{attempts}): {e:#}");
                tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop always returns on its last attempt")
}

/// Try a cached refresh token from a previous run first (one cheap
/// `ClientLogon` to confirm it's still valid, retried a few times via
/// [`with_retry`] before giving up on it), falling back to the full
/// negotiation if there isn't one cached, or a non-transient failure means
/// Steam has genuinely rejected it (revoked, expired, password changed,
/// ...). Either way, whatever refresh token ends up valid gets
/// (re-)persisted, so a successful cached-token reuse doesn't need to hit
/// the disk cache write path at all, and a fallback negotiation updates the
/// cache for the *next* restart.
async fn negotiate_or_reuse_refresh_token(
    username: &str,
    password: &str,
    install_dir: &Path,
) -> Result<String> {
    if let Some(cached) = cache::load_refresh_token(install_dir) {
        // Hand-rolled retry loop rather than with_retry -- this is the one
        // call site with no natural resource (like a CmConnection) to pass
        // through with_retry's per-attempt-reborrow signature; login_with_
        // refresh_token opens its own fresh connection internally.
        let mut result = login_with_refresh_token(username, &cached).await;
        for attempt in 1..DEFAULT_RETRY_ATTEMPTS {
            let Err(e) = &result else { break };
            if !is_transient(e) {
                break;
            }
            warn!(
                "transient error on cached Steam refresh token probe (attempt {attempt}/{DEFAULT_RETRY_ATTEMPTS}): {e:#}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
            result = login_with_refresh_token(username, &cached).await;
        }
        match result {
            Ok(mut conn) => {
                debug!("reused cached Steam refresh token, skipping full negotiation");
                let _ = conn.shutdown().await;
                return Ok(cached);
            }
            Err(e) => info!("cached Steam refresh token no longer valid ({e:#}), renegotiating"),
        }
    }

    let token = negotiate_refresh_token(username, password).await?;
    if let Err(e) = cache::save_refresh_token(install_dir, &token) {
        warn!("failed to cache Steam refresh token: {e:#}");
    }
    Ok(token)
}

/// Authenticate with Steam using the modern token-based flow and return a
/// connection with an established, logged-on session.
///
/// `auth::begin`/`poll` run over a connection that's already anonymously
/// logged on (required for the RSA/auth-session service calls) -- sending a
/// second `ClientLogon` with the refresh token on that same connection gets
/// the connection closed by the server. So the resulting tokens are used to
/// log on over a *fresh* connection instead, same as the CLI's separate
/// --username/--token path.
/// The expensive part of logging in: RSA key fetch, begin-auth-session,
/// poll -- all CM round-trips against a throwaway connection, ending in a
/// refresh token. This only needs to happen *once* per account, no matter
/// how many `CmConnection`s end up logged in to it -- the resulting token
/// is fully reusable via [`login_with_refresh_token`], which is a single
/// `ClientLogon` and nothing else. [`CmPool::start`] relies on this split
/// to avoid repeating the whole negotiation dance once per pool slot.
async fn negotiate_refresh_token(username: &str, password: &str) -> Result<String> {
    let (mut auth_conn, session) = steamdepot::auth::begin(username, password)
        .await
        .context("failed to begin Steam auth session")?;

    if session.needs_guard(GuardType::EmailCode) || session.needs_guard(GuardType::DeviceCode) {
        bail!(
            "Steam Guard confirmation required for this login (email/device code). \
             This account is expected to have Steam Guard disabled — check the account's \
             security settings, or wire up steam-mail if email codes are now required."
        );
    }

    let tokens = steamdepot::auth::poll(&mut auth_conn, &session)
        .await
        .context("failed while polling auth session status")?;
    let _ = auth_conn.shutdown().await;

    Ok(tokens.refresh_token)
}

/// Outcome of [`negotiate_interactive`] -- unlike [`negotiate_refresh_token`]
/// (which bails outright if Steam Guard is required, since it's only ever
/// called from a non-interactive process startup path), this lets the
/// caller actually complete a guard-gated login by prompting for a code
/// and calling again.
pub enum InteractiveAuthResult {
    /// No code was supplied and one is required -- call again with
    /// `guard_code` set to complete the login.
    NeedsGuard {
        guard_type: GuardType,
    },
    Success {
        refresh_token: String,
    },
}

/// Interactive credential (+ optional Steam Guard code) login, for
/// establishing a brand new session out-of-band -- an admin action run
/// once, rather than something a non-interactive process startup can do
/// on its own. The password is used only for the duration of this call;
/// nothing about it is persisted anywhere, only the resulting
/// `refresh_token`.
pub async fn negotiate_interactive(
    username: &str,
    password: &str,
    guard_code: Option<&str>,
) -> Result<InteractiveAuthResult> {
    let (mut conn, session) = auth::begin(username, password)
        .await
        .context("failed to begin Steam auth session")?;

    let needed_guard_type = if session.needs_guard(GuardType::EmailCode) {
        Some(GuardType::EmailCode)
    } else if session.needs_guard(GuardType::DeviceCode) {
        Some(GuardType::DeviceCode)
    } else {
        None
    };

    if let Some(guard_type) = needed_guard_type {
        match guard_code {
            None => {
                let _ = conn.shutdown().await;
                return Ok(InteractiveAuthResult::NeedsGuard { guard_type });
            }
            Some(code) => {
                auth::submit_guard(&mut conn, &session, code, guard_type)
                    .await
                    .context("failed to submit Steam Guard code")?;
            }
        }
    }

    let tokens = auth::poll(&mut conn, &session)
        .await
        .context("failed while polling auth session status")?;
    let _ = conn.shutdown().await;

    Ok(InteractiveAuthResult::Success {
        refresh_token: tokens.refresh_token,
    })
}

/// An in-progress QR login -- holds the CM connection the challenge was
/// issued on, since [`poll_qr_login`] polls over that same connection.
pub struct QrLoginSession {
    conn: CmConnection,
    session: steamdepot::login::AuthSession,
}

/// Begin a QR-code login: no password ever handled by this (or any other)
/// process at all. Returns the challenge URL to display/open alongside
/// the session -- scanning it with the Steam mobile app (or opening it
/// directly on a device that has the app installed, which deep-links)
/// confirms the login; [`poll_qr_login`] blocks until that happens.
pub async fn begin_qr_login() -> Result<(QrLoginSession, String)> {
    let (conn, session, challenge_url) = auth::begin_qr()
        .await
        .context("failed to begin Steam QR auth session")?;
    Ok((QrLoginSession { conn, session }, challenge_url))
}

/// Block until a QR login started by [`begin_qr_login`] is confirmed.
/// Returns `(account_name, refresh_token)` -- the account name is only
/// ever learned this way for a QR session (nothing was typed in to
/// already know it), unlike the credentials flow.
pub async fn poll_qr_login(mut qr: QrLoginSession) -> Result<(String, String)> {
    let tokens = auth::poll(&mut qr.conn, &qr.session)
        .await
        .context("failed while polling QR auth session status")?;
    let _ = qr.conn.shutdown().await;
    let username = tokens
        .account_name
        .ok_or_else(|| anyhow::anyhow!("Steam didn't return an account name after QR login"))?;
    Ok((username, tokens.refresh_token))
}

/// The cheap part: open a fresh connection and log on with an
/// already-negotiated refresh token -- one `ClientLogon`, no RSA/auth-
/// session/poll round-trips.
async fn login_with_refresh_token(username: &str, refresh_token: &str) -> Result<CmConnection> {
    let mut conn = connect_ws()
        .await
        .context("failed to open a fresh CM connection")?;

    login::login_with_token(&mut conn, username, refresh_token)
        .await
        .context("failed to log on with refresh token")?;

    conn.start_heartbeat();

    Ok(conn)
}

/// Log on anonymously. Works for the base server/Linux binary depots, but
/// Steam won't grant free CDLC licenses or workshop item access to
/// anonymous sessions -- see [`SteamAuth`].
pub async fn login_anonymous() -> Result<CmConnection> {
    let mut conn = connect_ws()
        .await
        .context("failed to open a fresh CM connection")?;
    login::login_anonymous(&mut conn)
        .await
        .context("anonymous login failed")?;
    conn.start_heartbeat();
    Ok(conn)
}

async fn connect_ws() -> Result<CmConnection> {
    let http = reqwest::Client::new();
    let cm_list = cm_list::get_cm_list(&http)
        .await
        .context("failed to fetch CM server list")?;

    let ws_server = cm_list
        .serverlist
        .iter()
        .find(|s| s.server_type == CmServerType::Websockets)
        .ok_or_else(|| SteamError::Other("no websocket CM server found".into()))?;

    CmConnection::connect(&ws_server.endpoint)
        .await
        .context("failed to connect to CM server")
}

/// Resolve every depot for app 233780 on the `creatordlc` branch, then keep
/// only the base server depots plus whichever CDLC depots were requested.
async fn resolve_wanted_depots(
    conn: &mut CmConnection,
    install_dir: &Path,
    include_profiling: bool,
    cdlc_depot_ids: &[u32],
    sync_state: &cache::SyncState,
) -> Result<DownloadPlan> {
    info!("Resolving depots for app {ARMA3_SERVER_APP_ID} (branch {TARGET_BRANCH})...");

    // depot::prepare_download() resolves *and fetches decryption keys for*
    // every depot on the branch (15-20+: all CDLCs, both binary variants,
    // symbols, ...) before we ever get to filter down to the handful we
    // actually want -- each key fetch is its own sequential CM round-trip,
    // so that's several seconds spent on depots we're about to throw away.
    // Filtering before fetching keys instead cuts straight to the ones we
    // need.
    let info = with_retry(
        format!("get_product_info(app {ARMA3_SERVER_APP_ID})"),
        DEFAULT_RETRY_ATTEMPTS,
        conn,
        |conn| {
            Box::pin(async move {
                with_timeout(
                    format!("get_product_info(app {ARMA3_SERVER_APP_ID})"),
                    pics::get_product_info(conn, &[ARMA3_SERVER_APP_ID], &[]),
                )
                .await
            })
        },
    )
    .await
    .context("failed to fetch product info")?;
    let app_info =
        info.apps.into_iter().next().ok_or_else(|| {
            anyhow::anyhow!("app {ARMA3_SERVER_APP_ID} not found in product info")
        })?;

    let all_depots = depot::resolve_depots(&app_info, TARGET_OS, TARGET_BRANCH)
        .context("failed to resolve depot list")?;
    debug!(
        "all depots on branch {TARGET_BRANCH} for os {TARGET_OS}: {:?}",
        all_depots
            .iter()
            .map(|d| (d.depot_id, d.depot_from_app))
            .collect::<Vec<_>>()
    );

    // 233781 = Default Content, 233783 = Linux Server, 233785 = Profiling
    let mut wanted: Vec<u32> = vec![233781, 233783];
    if include_profiling {
        wanted.push(233785);
    }
    wanted.extend_from_slice(cdlc_depot_ids);

    let depots: Vec<_> = all_depots
        .into_iter()
        .filter(|d| wanted.contains(&d.depot_id))
        .collect();

    let found: Vec<u32> = depots.iter().map(|d| d.depot_id).collect();
    for id in &wanted {
        if !found.contains(id) {
            warn!(
                "depot {} not found on branch {} for os {} (skipping)",
                id, TARGET_BRANCH, TARGET_OS
            );
        }
    }

    // Drop anything already fully synced at its current manifest_id before
    // spending a single CM round-trip (key fetch) or task-pool slot on it.
    // One snapshot query instead of one is_synced() call per depot -- see
    // SyncState::snapshot.
    let dir_exists = install_dir.is_dir();
    let synced_snapshot = sync_state.snapshot();
    let depots: Vec<_> = depots
        .into_iter()
        .filter(|d| {
            let key = cache::sync_key(d.depot_id, install_dir);
            if dir_exists && synced_snapshot.get(&key) == Some(&d.manifest_id) {
                info!(
                    "depot {} unchanged since last verified sync (manifest_id {}), trusting cache",
                    d.depot_id, d.manifest_id
                );
                false
            } else {
                true
            }
        })
        .collect();

    // Same free-license request prepare_download() does, just scoped to
    // the apps our wanted depots actually need.
    let mut license_apps = vec![ARMA3_SERVER_APP_ID];
    for d in &depots {
        if let Some(from) = d.depot_from_app {
            if !license_apps.contains(&from) {
                license_apps.push(from);
            }
        }
    }
    let is_authenticated = conn.session().map(|s| s.authenticated).unwrap_or(false);
    if is_authenticated {
        let result = with_retry(
            "request_free_license",
            DEFAULT_RETRY_ATTEMPTS,
            conn,
            |conn| {
                let license_apps = license_apps.clone();
                Box::pin(async move {
                    with_timeout(
                        "request_free_license",
                        pics::request_free_license(conn, &license_apps),
                    )
                    .await
                })
            },
        )
        .await;
        match result {
            Ok(resp) if !resp.granted_appids.is_empty() || !resp.granted_packageids.is_empty() => {
                info!(
                    "granted free licenses: apps={:?}, packages={:?}",
                    resp.granted_appids, resp.granted_packageids
                );
            }
            Ok(_) => {}
            Err(e) => warn!("free license request failed: {e}"),
        }
    }

    // Depot keys never rotate for a given depot_id, so a cache hit skips
    // the CM round-trip entirely. Saved after each new key individually
    // (not batched at the end) so a crash partway through resolution --
    // e.g. mid-fetch on a large CDLC list -- doesn't throw away keys
    // already obtained; a retry picks up from where it left off instead
    // of re-fetching everything.
    let mut key_cache = cache::load_keys(install_dir);

    let mut plans = Vec::new();
    for depot in depots {
        let key = if let Some(cached) = key_cache.get(&depot.depot_id) {
            debug!("depot {} key: cache hit", depot.depot_id);
            cached.clone()
        } else {
            let key_app = depot.containing_app(ARMA3_SERVER_APP_ID);
            let fetched = with_retry(
                format!("get_depot_decryption_key(depot {})", depot.depot_id),
                DEFAULT_RETRY_ATTEMPTS,
                conn,
                |conn| {
                    Box::pin(async move {
                        with_timeout(
                            format!("get_depot_decryption_key(depot {})", depot.depot_id),
                            pics::get_depot_decryption_key(conn, depot.depot_id, key_app),
                        )
                        .await
                    })
                },
            )
            .await;
            let key = match fetched {
                Ok(key) => key,
                Err(e) => {
                    // One depot (e.g. a CDLC the account doesn't actually
                    // have access to for some transient reason) failing to
                    // resolve shouldn't block every other depot, let alone
                    // fail server startup outright.
                    warn!(
                        "depot {} failed to get depot key, skipping: {e:#}",
                        depot.depot_id
                    );
                    continue;
                }
            };
            key_cache.insert(depot.depot_id, key.clone());
            if let Err(e) = cache::save_keys(install_dir, &key_cache) {
                warn!("failed to save depot key cache: {e}");
            }
            key
        };
        plans.push(DepotPlan {
            depot,
            key,
            manifest_request_code: None,
            manifest: None,
        });
    }

    info!("Resolved {} depot(s)", plans.len());

    Ok(DownloadPlan {
        app_id: ARMA3_SERVER_APP_ID,
        plans,
        cdn_servers: Vec::new(),
        granted_appids: Vec::new(),
        granted_packageids: Vec::new(),
        cdn_auth_tokens: Vec::new(),
    })
}

/// Resolve the Arma 3 dedicated server's depots (default content, linux
/// server binary, and any requested CDLC depots), then spawn their
/// downloads into the shared `tasks` pool -- does not wait for them to
/// finish, so the caller can move straight on to resolving other content
/// (e.g. workshop mods) on the same connection while these download in the
/// background.
pub async fn resolve_and_spawn_server(
    conn: &mut CmConnection,
    install_dir: &Path,
    include_profiling: bool,
    cdlc_depot_ids: &[u32],
    sem: Arc<Semaphore>,
    tasks: &Mutex<SyncTasks>,
    sync_state: Arc<cache::SyncState>,
) -> Result<()> {
    let plan = resolve_wanted_depots(
        conn,
        install_dir,
        include_profiling,
        cdlc_depot_ids,
        &sync_state,
    )
    .await?;

    // An empty plan here means everything wanted was already fully synced
    // (see resolve_wanted_depots' cache filter) -- resolve_wanted_depots
    // already warns if a *wanted* depot wasn't found on the branch at all,
    // so there's nothing further to check or bail on.
    if plan.plans.is_empty() {
        return Ok(());
    }

    spawn_plan_downloads(
        conn,
        plan,
        TARGET_BRANCH,
        install_dir,
        sem,
        tasks,
        sync_state,
    )
    .await
}

/// Fetch manifests for an already-resolved plan, then spawn each depot's
/// download into the shared task pool. Shared by both the server/CDLC path
/// (depots resolved via PICS) and the workshop path (depots resolved via
/// `IPublishedFileService.GetDetails`).
async fn spawn_plan_downloads(
    conn: &mut CmConnection,
    plan: DownloadPlan,
    branch: &str,
    install_dir: &Path,
    sem: Arc<Semaphore>,
    tasks: &Mutex<SyncTasks>,
    sync_state: Arc<cache::SyncState>,
) -> Result<()> {
    let http = reqwest::Client::new();
    let app_id = plan.app_id;

    // Same split as workshop resolution: manifest-request-codes are
    // CM-bound and must stay sequential (one connection), but the actual
    // manifest *bytes* download is plain CDN HTTP and doesn't need to wait
    // for its neighbors -- was previously the crate's fetch_manifests(),
    // which does both steps sequentially per depot.
    let cdn_servers = with_retry("get_cdn_servers", DEFAULT_RETRY_ATTEMPTS, conn, |conn| {
        Box::pin(async move {
            with_timeout(
                "get_cdn_servers",
                steamdepot::cdn::get_cdn_servers(
                    conn,
                    conn.session().map(|s| s.cell_id).unwrap_or(0),
                ),
            )
            .await
        })
    })
    .await
    .context("failed to fetch CDN server list")?;
    if cdn_servers.is_empty() {
        bail!("no CDN servers available");
    }
    let server = cdn_servers[0].clone();

    // manifest_id here always comes from this run's fresh get_product_info
    // resolution (never cached), so a cache hit means "Steam's current
    // manifest_id for this depot matches one we already have" -- an
    // update always produces a different manifest_id, which is a cache
    // miss by construction, not something the cache can paper over.
    let mut cache_hits = Vec::new();
    let mut needs_fetch = Vec::new();
    for dp in plan.plans {
        match cache::load_manifest(install_dir, dp.depot.depot_id, dp.depot.manifest_id) {
            Some(manifest) => {
                debug!(
                    "depot {} manifest: cache hit (manifest_id {})",
                    dp.depot.depot_id, dp.depot.manifest_id
                );
                let mut dp = dp;
                dp.manifest = Some(manifest);
                cache_hits.push(dp);
            }
            None => needs_fetch.push(dp),
        }
    }

    let mut pending = Vec::new();
    for dp in needs_fetch {
        let result = with_retry(
            format!("get_manifest_request_code(depot {})", dp.depot.depot_id),
            DEFAULT_RETRY_ATTEMPTS,
            conn,
            |conn| {
                let depot = dp.depot.clone();
                let branch = branch.to_string();
                Box::pin(async move {
                    with_timeout(
                        format!("get_manifest_request_code(depot {})", depot.depot_id),
                        steamdepot::cdn::get_manifest_request_code(
                            conn,
                            depot.containing_app(app_id),
                            depot.depot_id,
                            depot.manifest_id,
                            &branch,
                        ),
                    )
                    .await
                })
            },
        )
        .await;
        match result {
            Ok(code) => pending.push((dp, code)),
            // Same reasoning as resolve_wanted_depots' key-fetch skip above:
            // one depot failing here (transient Steam-side issue, access
            // hiccup right after a content update, ...) shouldn't cost the
            // rest of the batch or crash server startup.
            Err(e) => warn!(
                "depot {} failed to get manifest request code, skipping: {e:#}",
                dp.depot.depot_id
            ),
        }
    }

    let mut manifest_tasks = FuturesUnordered::new();
    for (mut dp, code) in pending {
        let http = http.clone();
        let server = server.clone();
        let install_dir = install_dir.to_path_buf();
        manifest_tasks.push(async move {
            let manifest = steamdepot::cdn::download_manifest(
                &http,
                &server,
                dp.depot.depot_id,
                dp.depot.manifest_id,
                code,
                &dp.key,
                None,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to download manifest for depot {}",
                    dp.depot.depot_id
                )
            })?;
            if let Err(e) = cache::save_manifest(
                &install_dir,
                dp.depot.depot_id,
                dp.depot.manifest_id,
                &manifest,
            ) {
                warn!(
                    "failed to cache manifest for depot {}: {e}",
                    dp.depot.depot_id
                );
            }
            dp.manifest_request_code = Some(code);
            dp.manifest = Some(manifest);
            Ok::<_, anyhow::Error>(dp)
        });
    }

    let mut resolved_plans = cache_hits;
    while let Some(result) = manifest_tasks.next().await {
        match result {
            Ok(dp) => resolved_plans.push(dp),
            Err(e) => warn!("skipping a depot after a manifest fetch failure: {e:#}"),
        }
    }

    let pool = Arc::new(Mutex::new(CdnPool::new(cdn_servers)));

    for dp in resolved_plans {
        let fut = download_one_depot(
            dp,
            install_dir.to_path_buf(),
            http.clone(),
            pool.clone(),
            sync_state.clone(),
        );
        spawn_bounded(tasks, sem.clone(), fut);
    }

    Ok(())
}

/// A workshop item resolved down to a downloadable, manifest-fetched depot
/// plan. Deliberately doesn't get downloaded/spawned by
/// [`resolve_workshop_items`] itself, since only the caller (`workshop.rs`)
/// knows what post-processing (metadata patching, cached gid bookkeeping)
/// needs to happen once a given item's download completes.
pub struct ResolvedWorkshopItem {
    pub published_file_id: u64,
    pub plan: DepotPlan,
}

/// Everything needed to download a batch of already-resolved workshop items:
/// the items themselves plus the CDN server pool their manifests were
/// fetched from (reused for the chunk downloads too, so there's no need to
/// look CDN servers up a second time).
pub struct WorkshopResolution {
    pub items: Vec<ResolvedWorkshopItem>,
    pub cdn_pool: Arc<Mutex<CdnPool>>,
}

/// Resolve a batch of workshop items (manifest fetch included) without
/// downloading anything.
///
/// `conn` is used for the initial GetDetails + per-item key/manifest-code
/// requests, which must happen one at a time (a `CmConnection` isn't
/// shareable across tasks); the manifest *bytes* fetch from CDN afterward is
/// plain HTTP and runs concurrently across items.
pub async fn resolve_workshop_items(
    conn: &mut CmConnection,
    install_dir: &Path,
    published_file_ids: &[u64],
    sync_state: &cache::SyncState,
) -> Result<WorkshopResolution> {
    if published_file_ids.is_empty() {
        return Ok(WorkshopResolution {
            items: Vec::new(),
            cdn_pool: Arc::new(Mutex::new(CdnPool::new(Vec::new()))),
        });
    }

    info!("Resolving {} workshop item(s)...", published_file_ids.len());
    let items = with_retry(
        format!("workshop::get_details({} items)", published_file_ids.len()),
        DEFAULT_RETRY_ATTEMPTS,
        conn,
        |conn| {
            let published_file_ids = published_file_ids.to_vec();
            Box::pin(async move {
                with_timeout(
                    format!("workshop::get_details({} items)", published_file_ids.len()),
                    steamdepot::workshop::get_details(conn, &published_file_ids),
                )
                .await
            })
        },
    )
    .await
    .context("failed to resolve workshop items")?;
    info!(
        "Resolved {} workshop item(s), fetching manifest request codes...",
        items.len()
    );

    // Every Arma 3 workshop item shares depot_id == consumer_appid (the
    // whole workshop is one "depot" for the app), so without dedup this
    // loop would fetch the exact same key up to len(items) times. Keyed
    // cache (memory + disk, keys never rotate) collapses that to once per
    // unique consumer app, typically once total.
    let mut key_cache = cache::load_keys(install_dir);

    // Phase 1 (sequential -- both calls go over the single authenticated
    // CmConnection, which is stateful and can't be shared across concurrent
    // requests): get each item's depot key (cache permitting) and manifest
    // request code (skipped entirely on a manifest cache hit).
    // One snapshot query instead of one is_synced() call per item -- see
    // SyncState::snapshot.
    let synced_snapshot = sync_state.snapshot();
    let mut pending = Vec::new();
    let mut cache_hits = Vec::new();
    let mut already_synced = 0usize;
    let phase1_start = std::time::Instant::now();
    let mut load_manifest_total = std::time::Duration::ZERO;
    for (i, item) in items.iter().enumerate() {
        let item_dir = install_dir.join(item.published_file_id.to_string());
        let sync_key = cache::sync_key(item.consumer_appid, &item_dir);
        if item_dir.is_dir() && synced_snapshot.get(&sync_key) == Some(&item.manifest_id) {
            info!(
                "[{}] manifest unchanged since last verified sync (manifest_id {}), trusting cache (skipping dispatch)",
                item.published_file_id, item.manifest_id
            );
            already_synced += 1;
            continue;
        }

        let load_start = std::time::Instant::now();
        let cached_manifest =
            cache::load_manifest(install_dir, item.consumer_appid, item.manifest_id);
        load_manifest_total += load_start.elapsed();
        if let Some(manifest) = cached_manifest {
            debug!(
                "[{}] manifest: cache hit (manifest_id {})",
                item.published_file_id, item.manifest_id
            );
            let key = if let Some(cached) = key_cache.get(&item.consumer_appid) {
                cached.clone()
            } else {
                let fetched = with_retry(
                    format!(
                        "get_depot_decryption_key(workshop item {})",
                        item.published_file_id
                    ),
                    DEFAULT_RETRY_ATTEMPTS,
                    conn,
                    |conn| {
                        let published_file_id = item.published_file_id;
                        let consumer_appid = item.consumer_appid;
                        Box::pin(async move {
                            with_timeout(
                                format!(
                                    "get_depot_decryption_key(workshop item {published_file_id})"
                                ),
                                pics::get_depot_decryption_key(
                                    conn,
                                    consumer_appid,
                                    consumer_appid,
                                ),
                            )
                            .await
                        })
                    },
                )
                .await;
                let key = match fetched {
                    Ok(key) => key,
                    Err(e) => {
                        // Every workshop item shares this same depot key, so
                        // a failure here will hit every remaining item too
                        // -- but that's still strictly better than crashing
                        // the whole launcher over it (see the
                        // manifest-request-code failure below for why this
                        // whole loop tolerates per-item failures now).
                        warn!(
                            "[{}] failed to get depot key, skipping: {e:#}",
                            item.published_file_id
                        );
                        continue;
                    }
                };
                key_cache.insert(item.consumer_appid, key.clone());
                if let Err(e) = cache::save_keys(install_dir, &key_cache) {
                    warn!("failed to save workshop depot key cache: {e}");
                }
                key
            };
            cache_hits.push(ResolvedWorkshopItem {
                published_file_id: item.published_file_id,
                plan: DepotPlan {
                    depot: depot::DepotInfo {
                        depot_id: item.consumer_appid,
                        manifest_id: item.manifest_id,
                        depot_from_app: None,
                    },
                    key,
                    manifest_request_code: None,
                    manifest: Some(manifest),
                },
            });
            continue;
        }

        debug!(
            "[{}/{}] [{}] requesting depot key + manifest code...",
            i + 1,
            items.len(),
            item.published_file_id
        );

        let key = if let Some(cached) = key_cache.get(&item.consumer_appid) {
            cached.clone()
        } else {
            let fetched = with_retry(
                format!(
                    "get_depot_decryption_key(workshop item {})",
                    item.published_file_id
                ),
                DEFAULT_RETRY_ATTEMPTS,
                conn,
                |conn| {
                    let published_file_id = item.published_file_id;
                    let consumer_appid = item.consumer_appid;
                    Box::pin(async move {
                        with_timeout(
                            format!("get_depot_decryption_key(workshop item {published_file_id})"),
                            pics::get_depot_decryption_key(conn, consumer_appid, consumer_appid),
                        )
                        .await
                    })
                },
            )
            .await;
            let key = match fetched {
                Ok(key) => key,
                Err(e) => {
                    warn!(
                        "[{}] failed to get depot key, skipping: {e:#}",
                        item.published_file_id
                    );
                    continue;
                }
            };
            key_cache.insert(item.consumer_appid, key.clone());
            if let Err(e) = cache::save_keys(install_dir, &key_cache) {
                warn!("failed to save workshop depot key cache: {e}");
            }
            key
        };

        // A single item failing here (observed live: EResult 15 AccessDenied
        // right after the item's owner pushed an update -- plausibly Steam's
        // CDN/manifest-request-code system needing a moment to catch up with
        // a just-published manifest_id) must not take down every other mod
        // in the batch, let alone the whole server. Skip it, log clearly,
        // and let it resolve on some later run once whatever's going on
        // upstream clears up -- the resolve-time sync_state check means a
        // skipped item here simply won't be in `items` next run either, so
        // it keeps getting retried for free on every restart until it
        // succeeds.
        let request_code = match with_retry(
            format!(
                "get_manifest_request_code(workshop item {})",
                item.published_file_id
            ),
            DEFAULT_RETRY_ATTEMPTS,
            conn,
            |conn| {
                let published_file_id = item.published_file_id;
                let consumer_appid = item.consumer_appid;
                let manifest_id = item.manifest_id;
                Box::pin(async move {
                    with_timeout(
                        format!("get_manifest_request_code(workshop item {published_file_id})"),
                        steamdepot::cdn::get_manifest_request_code(
                            conn,
                            consumer_appid,
                            consumer_appid,
                            manifest_id,
                            "public",
                        ),
                    )
                    .await
                })
            },
        )
        .await
        {
            Ok(code) => code,
            Err(e) => {
                warn!(
                    "[{}] failed to get manifest request code, skipping this item for now: {e:#}",
                    item.published_file_id
                );
                continue;
            }
        };

        pending.push((item.clone(), key, request_code));
    }
    info!(
        "phase 1 done in {:.1}s ({} already synced (skipped entirely), {} cache hits, {} need fetch; cache::load_manifest calls took {:.1}s of that total)",
        phase1_start.elapsed().as_secs_f64(),
        already_synced,
        cache_hits.len(),
        pending.len(),
        load_manifest_total.as_secs_f64(),
    );

    // Phase 2 (parallel -- plain CDN HTTP, no CmConnection involved): fetch
    // each item's manifest bytes concurrently, for whatever wasn't already
    // a cache hit above.
    info!(
        "Fetching {} manifest(s) from CDN ({} already cached)...",
        pending.len(),
        cache_hits.len()
    );
    let cdn_servers = with_retry("get_cdn_servers", DEFAULT_RETRY_ATTEMPTS, conn, |conn| {
        Box::pin(async move {
            with_timeout(
                "get_cdn_servers",
                steamdepot::cdn::get_cdn_servers(
                    conn,
                    conn.session().map(|s| s.cell_id).unwrap_or(0),
                ),
            )
            .await
        })
    })
    .await
    .context("failed to fetch CDN server list")?;
    if cdn_servers.is_empty() {
        bail!("no CDN servers available");
    }
    let server = cdn_servers[0].clone();
    let http = reqwest::Client::new();

    let mut manifest_tasks = FuturesUnordered::new();
    for (item, key, request_code) in pending {
        let http = http.clone();
        let server = server.clone();
        let install_dir = install_dir.to_path_buf();
        manifest_tasks.push(async move {
            let manifest = steamdepot::cdn::download_manifest(
                &http,
                &server,
                item.consumer_appid,
                item.manifest_id,
                request_code,
                &key,
                None,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to fetch manifest for workshop item {}",
                    item.published_file_id
                )
            })?;
            info!("[{}] manifest ready", item.published_file_id);
            if let Err(e) = cache::save_manifest(
                &install_dir,
                item.consumer_appid,
                item.manifest_id,
                &manifest,
            ) {
                warn!(
                    "failed to cache manifest for workshop item {}: {e}",
                    item.published_file_id
                );
            }

            Ok::<_, anyhow::Error>(ResolvedWorkshopItem {
                published_file_id: item.published_file_id,
                plan: DepotPlan {
                    depot: depot::DepotInfo {
                        depot_id: item.consumer_appid,
                        manifest_id: item.manifest_id,
                        depot_from_app: None,
                    },
                    key,
                    manifest_request_code: Some(request_code),
                    manifest: Some(manifest),
                },
            })
        });
    }

    // Same reasoning as the phase 1 skips above: one item's manifest bytes
    // failing to fetch from the CDN shouldn't take the rest of the batch
    // (or the server) down with it.
    let mut resolved = cache_hits;
    while let Some(result) = manifest_tasks.next().await {
        match result {
            Ok(item) => resolved.push(item),
            Err(e) => warn!("skipping a workshop item after a manifest fetch failure: {e:#}"),
        }
    }

    Ok(WorkshopResolution {
        items: resolved,
        cdn_pool: Arc::new(Mutex::new(CdnPool::new(cdn_servers))),
    })
}

pub(crate) async fn download_one_depot(
    mut dp: DepotPlan,
    install_dir: std::path::PathBuf,
    http: reqwest::Client,
    pool: Arc<Mutex<CdnPool>>,
    sync_state: Arc<cache::SyncState>,
) -> Result<()> {
    let manifest = dp.manifest.as_mut().with_context(|| {
        format!(
            "depot {} has no manifest after fetch_manifests",
            dp.depot.depot_id
        )
    })?;

    decrypt_manifest_filenames(manifest, &dp.key).with_context(|| {
        format!(
            "failed to decrypt filenames for depot {}",
            dp.depot.depot_id
        )
    })?;

    let depot_id = dp.depot.depot_id;
    // depot_id alone doesn't uniquely identify a download: every workshop
    // item shares depot_id == its consumer app (107410 for Arma 3), so
    // several mods downloading at once would otherwise all log as the same
    // "[depot 107410]". install_dir's last component disambiguates them
    // (it's the published file ID for mods, redundant-but-harmless for
    // server depots where it's always the shared server root).
    let leaf = install_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?");
    let tag = format!("depot {depot_id}/{leaf}");
    let manifest_id = dp.depot.manifest_id;
    let sync_key = cache::sync_key(depot_id, &install_dir);

    // More than one server instance can share this content directory, so
    // the resolve-time skip check alone isn't enough -- another instance
    // could be mid-sync on this exact key right now, or could start one
    // concurrently with us. Hold the cross-process lock for the rest of
    // this function so only one instance ever verifies/downloads a given
    // depot/mod's files at a time.
    let _lock = sync_state.acquire_lock(sync_key.clone()).await?;

    // Re-check after acquiring the lock: if another instance just finished
    // syncing this exact manifest_id while we were waiting, trust its work
    // instead of redundantly re-verifying everything ourselves.
    if sync_state.is_synced(&sync_key, manifest_id, install_dir.is_dir()) {
        info!(
            "[{tag}] another server instance already synced this manifest_id ({manifest_id}) while we waited, trusting it"
        );
        return Ok(());
    }

    // Reaching this point means this depot/mod genuinely needs a real
    // verification pass -- either it's never been synced, or its
    // manifest_id no longer matches what was last verified on disk (a
    // genuine content update). Log which, purely for visibility.
    match sync_state.last_manifest_id(&sync_key) {
        Some(old) => info!(
            "[{tag}] manifest_id changed since last verified sync ({old} -> {manifest_id}), content was updated -- verifying"
        ),
        None => info!(
            "[{tag}] no previous synced-manifest marker (manifest_id {manifest_id}), verifying"
        ),
    }

    info!(
        "[{tag}] {} files, verifying against manifest in {}",
        manifest.payload.mappings.len(),
        install_dir.display()
    );

    let last_pct = AtomicU32::new(u32::MAX);
    let tag_progress = tag.clone();
    // sync_depot always prepares the directory tree and verifies existing
    // chunks against the manifest before downloading anything -- there's no
    // lower-level call that skips straight to downloading.
    let (_prepared, result) = download::sync_depot(
        &http,
        pool,
        depot_id,
        manifest,
        &dp.key,
        &install_dir,
        DOWNLOAD_WORKERS,
        move |p| {
            // Log on whole-percent thresholds only -- per-chunk would be
            // way too noisy for a real log stream.
            let pct = if p.chunks_total > 0 {
                ((p.chunks_done as f64 / p.chunks_total as f64) * 100.0) as u32
            } else {
                0
            };
            let prev = last_pct.load(Ordering::Relaxed);
            if prev == u32::MAX || pct > prev {
                if last_pct
                    .compare_exchange(prev, pct, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    info!(
                        "[{tag_progress}] {}%  chunks {}/{} (verified {})  bytes {}",
                        pct, p.chunks_done, p.chunks_total, p.chunks_verified, p.bytes_downloaded
                    );
                }
            }
        },
    )
    .await
    .with_context(|| format!("failed to sync depot {}", depot_id))?;

    info!(
        "[{tag}] done: {} chunks ({} already valid, {} downloaded), {} bytes",
        result.chunks_done,
        result.chunks_verified,
        result.chunks_done - result.chunks_verified,
        result.bytes_downloaded
    );

    // Depot manifests come from Steam's Windows-built content and can carry
    // mixed-case filenames; Arma 3 on Linux refuses to load non-lowercase
    // PBOs. Fix this up after every real sync (not just first-time
    // downloads -- an updated manifest can reintroduce mixed-case names)
    // and before mark_synced, so nothing is ever recorded as synced with
    // filenames Arma can't actually load.
    lowercase_synced_tree(&install_dir).await.with_context(|| {
        format!(
            "failed to lowercase filenames under {}",
            install_dir.display()
        )
    })?;

    // The dedicated server depot's manifest doesn't set FLAG_EXECUTABLE on
    // its own server binaries (confirmed live: steamdepot's download.rs
    // does chmod +x when that flag is set, and it wasn't -- the binary
    // landed 0o644, "Permission denied" on exec). A Windows-authored
    // depot's Linux-executable metadata apparently can't be trusted here,
    // so fix up the known binary names explicitly, same fixup-after-sync
    // pattern as lowercase_synced_tree above.
    fix_executable_bits(&install_dir).await.with_context(|| {
        format!(
            "failed to set executable bits under {}",
            install_dir.display()
        )
    })?;

    let size_bytes = dir_size(&install_dir).await.unwrap_or_else(|e| {
        warn!("[{tag}] failed to compute on-disk size, recording 0: {e:#}");
        0
    });
    sync_state.mark_synced(&sync_key, manifest_id, size_bytes);

    Ok(())
}

/// Renames every file/directory under `dir` whose name isn't already
/// lowercase. Walks deepest-entries-first so renaming a directory never
/// invalidates a path this same pass still needs to visit.
async fn lowercase_synced_tree(dir: &std::path::Path) -> Result<()> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || lowercase_tree_blocking(&dir))
        .await
        .context("lowercase-filenames task panicked")?
}

fn lowercase_tree_blocking(dir: &std::path::Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(dir)
        .contents_first(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_lowercase();
        if lower == name {
            continue;
        }

        let dest = path.with_file_name(&lower);
        if dest.exists() {
            // Two siblings differing only by case (rare, but possible in a
            // sloppily-packaged mod) would otherwise clobber each other.
            warn!(
                "[{}] not renaming {name} -> {lower}: a file with that name already exists",
                dir.display()
            );
            continue;
        }
        std::fs::rename(path, &dest)
            .with_context(|| format!("failed to rename {} to {lower}", path.display()))?;
    }
    Ok(())
}

/// Total bytes of every regular file under `dir`, recursively -- used to
/// record a synced depot/mod's on-disk footprint alongside its
/// verified-manifest marker, so disk-usage queries are a plain read of
/// `SyncState` rather than a live directory walk on every call.
async fn dir_size(dir: &std::path::Path) -> Result<u64> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        Ok(walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum())
    })
    .await
    .context("dir_size task panicked")?
}

/// Known Linux server binary names Arma 3's dedicated server depot
/// ships (lowercased -- runs after `lowercase_synced_tree`), whose
/// executable bit needs setting explicitly. See the caller's comment for
/// why this can't just rely on the manifest's own executable flag.
#[cfg_attr(not(unix), allow(dead_code))] // only read inside fix_executable_bits_blocking's #[cfg(unix)] variant
const KNOWN_SERVER_BINARIES: &[&str] = &["arma3server", "arma3server_x64"];

async fn fix_executable_bits(dir: &std::path::Path) -> Result<()> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || fix_executable_bits_blocking(&dir))
        .await
        .context("chmod task panicked")?
}

#[cfg(unix)]
fn fix_executable_bits_blocking(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !KNOWN_SERVER_BINARIES.contains(&name) {
            continue;
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to chmod +x {}", path.display()))?;
    }
    Ok(())
}

// Windows has no executable-bit concept to fix -- KNOWN_SERVER_BINARIES
// are Linux server binaries in the first place (see its own doc), so
// this genuinely never has anything to do there, not just "doesn't
// bother."
#[cfg(not(unix))]
fn fix_executable_bits_blocking(_dir: &std::path::Path) -> Result<()> {
    Ok(())
}

/// Depth guard against pathological/cyclic collection nesting -- Steam
/// Workshop collections aren't normally nested at all, but nothing stops a
/// malformed or malicious one from referencing another collection as a
/// "child", so this bounds how deep `resolve_source_ids` will follow that
/// rather than trusting Steam's data to be well-formed.
const MAX_COLLECTION_DEPTH: u32 = 8;

/// Steamworks SDK's `EWorkshopFileType::k_EWorkshopFileTypeCollection`.
const WORKSHOP_FILE_TYPE_COLLECTION: u32 = 2;

/// Resolve a batch of candidate Steam Workshop IDs -- each may be an
/// individual mod or a collection, indistinguishable without asking Steam
/// -- into a flat list of individual mod IDs, expanding any collections
/// found into their member mods.
///
/// Goes through the *authenticated* `PublishedFile.GetDetails` call (same
/// one `resolve_workshop_items` uses), requesting `includechildren` so
/// Steam includes a collection's member list directly in the response --
/// no second RPC needed. Doing this over the authenticated CM session
/// rather than Steam's public, unauthenticated Web API is what makes this
/// correctly resolve unlisted and private mods/collections: a plain HTTP
/// call only sees what's publicly visible, but this sees whatever the
/// logged-in account itself has access to.
///
/// Whether a given ID is a *pure* collection (no depot content of its
/// own, only expand its members) is decided by `file_type ==
/// WORKSHOP_FILE_TYPE_COLLECTION`, not by whether `children` is
/// non-empty -- confirmed live: a plain Mod with required items (e.g. one
/// depending on CBA_A3) also populates `children` with those
/// dependencies. Only a real collection's `children` get expanded into
/// the next frontier; a plain mod's own `children` (its declared Required
/// Items) are deliberately left alone -- auto-pulling a mod's
/// dependencies gave an operator no way to pin a fork/specific version of
/// one instead (e.g. a custom CBA_A3 build) for anything that declares it
/// as required. A dependency an operator actually wants synced has to be
/// registered as its own separate mod source, same as any other mod.
pub async fn resolve_source_ids(
    conn: &mut CmConnection,
    candidate_ids: &[u64],
) -> Result<ResolveOutcome> {
    let mut resolved: std::collections::HashMap<u64, (String, u64)> =
        std::collections::HashMap::new();
    // The originally-requested candidates' own titles (a mod's own title,
    // or a collection's -- collections carry their own `title` even though
    // `children` is what actually gets expanded). Only ever populated from
    // the first request round (depth 1 below), since that's the only round
    // that ever sees an original candidate ID.
    let mut candidate_titles: std::collections::HashMap<u64, String> =
        std::collections::HashMap::new();
    let mut candidate_is_collection: std::collections::HashMap<u64, bool> =
        std::collections::HashMap::new();
    let mut frontier: Vec<u64> = candidate_ids.to_vec();
    let mut depth = 0;

    while !frontier.is_empty() {
        if depth >= MAX_COLLECTION_DEPTH {
            warn!(
                "hit max collection nesting depth ({MAX_COLLECTION_DEPTH}) resolving {:?}, stopping expansion",
                frontier
            );
            break;
        }
        depth += 1;

        let req = steamdepot::proto::CPublishedFileGetDetailsRequest {
            publishedfileids: frontier.clone(),
            includechildren: Some(true),
            ..Default::default()
        };
        let resp_bytes = with_retry(
            format!(
                "PublishedFile.GetDetails(includechildren, {} ids)",
                frontier.len()
            ),
            DEFAULT_RETRY_ATTEMPTS,
            conn,
            |conn| {
                let req = req.clone();
                Box::pin(async move {
                    with_timeout(
                        "PublishedFile.GetDetails(includechildren)",
                        conn.service_method_call(
                            "PublishedFile.GetDetails#1",
                            &prost::Message::encode_to_vec(&req),
                        ),
                    )
                    .await
                })
            },
        )
        .await?;
        let resp = <steamdepot::proto::CPublishedFileGetDetailsResponse as prost::Message>::decode(
            resp_bytes.as_slice(),
        )
        .context("failed to decode PublishedFile.GetDetails response")?;

        let mut next_frontier = Vec::new();
        for details in resp.publishedfiledetails {
            let id = details.publishedfileid.unwrap_or(0);
            let eresult = details.result.unwrap_or(0);
            if eresult != 1 {
                warn!(
                    "workshop item {id} GetDetails returned eresult {eresult} (skipping -- may be private/unlisted/removed)"
                );
                continue;
            }
            let title = details.title.clone().unwrap_or_default();
            let file_type = details.file_type.unwrap_or(0);
            if depth == 1 {
                candidate_titles.insert(id, title.clone());
                candidate_is_collection.insert(id, file_type == WORKSHOP_FILE_TYPE_COLLECTION);
            }
            if file_type == WORKSHOP_FILE_TYPE_COLLECTION {
                // Only a real collection's children get expanded --
                // deliberately not a plain mod's own `children` (Steam
                // populates that identically for a mod's declared
                // Required Items, e.g. ACE3 listing CBA_A3). Auto-pulling
                // those in was silently forcing whatever CBA_A3 build
                // Steam resolves, with no way for an operator to pin a
                // fork/specific version instead -- a dependency an
                // operator actually wants has to be registered as its
                // own separate mod source now, same as any other mod.
                for child in details.children {
                    if let Some(child_id) = child.publishedfileid {
                        next_frontier.push(child_id);
                    }
                }
            } else {
                // 0 (not just missing) for a collection's own entry, which
                // never reaches here anyway -- only genuinely unset for a
                // handful of legacy items GetDetails doesn't report a size
                // for at all, treated as "unknown" (0).
                let file_size = details.file_size.unwrap_or(0);
                resolved.insert(id, (title, file_size));
            }
        }
        frontier = next_frontier;
    }

    Ok(ResolveOutcome {
        mods: resolved
            .into_iter()
            .map(|(mod_id, (title, file_size))| ResolvedMod {
                mod_id,
                title,
                file_size,
            })
            .collect(),
        candidate_titles,
        candidate_is_collection,
    })
}

pub struct ResolvedMod {
    pub mod_id: u64,
    pub title: String,
    /// From PublishedFileDetails.file_size -- 0 if Steam didn't report one
    /// (see resolve_source_ids' own comment on this field).
    pub file_size: u64,
}

pub struct ResolveOutcome {
    pub mods: Vec<ResolvedMod>,
    pub candidate_titles: std::collections::HashMap<u64, String>,
    /// Which of the originally-requested `candidate_ids` are themselves a
    /// *pure* Steam Workshop collection (`file_type ==
    /// WORKSHOP_FILE_TYPE_COLLECTION`), keyed by id -- only ever populated
    /// from the first request round (depth 1), same as `candidate_titles`.
    /// The real signal for "is this a collection", as opposed to comparing
    /// `mods` against `candidate_ids`: that comparison also changes shape
    /// for a plain mod with required items (e.g. ACE3 depending on
    /// CBA_A3), which populates `children` (and so changes the resolved
    /// set) without the candidate itself being a collection at all.
    pub candidate_is_collection: std::collections::HashMap<u64, bool>,
}
