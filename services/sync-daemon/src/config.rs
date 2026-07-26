use std::env;
use std::path::PathBuf;

use anyhow::Result;

/// What this process was told about Steam auth at boot, before ever
/// touching the session Secret (see `secrets.rs`) -- `main.rs` combines
/// this with whatever's actually in that Secret to decide the real
/// `SteamAuth` to start `CmPool` with, so a stored session always takes
/// priority over this when both are present. Deliberately has no
/// credentials-at-startup variant at all -- a Steam password should never
/// reach this (or any other deployed) process, not even via an env var
/// sourced from a Secret. The only ways to get a session running are
/// `Anonymous` or an already-negotiated refresh token, established
/// out-of-band via `RefreshSteamAuth` (see its own proto doc) and read
/// back from the session Secret below.
pub enum SteamAuthConfig {
    /// Can sync already-public workshop mods, but can never sync the
    /// base game/CDLC depots at all -- Steam's RequestFreeLicense
    /// (what makes those downloadable without actually owning Arma 3)
    /// is refused for anonymous sessions outright, confirmed live via
    /// steamcmd's own `app_license_request` ("Not for anonymous
    /// users"). A real deployment always needs a real, non-anonymous
    /// account (doesn't need to own Arma 3 either) via
    /// `RefreshSteamAuth`/`magpiectl admin refresh-steam-auth`
    /// regardless of this setting.
    Anonymous,
    /// Anonymous login isn't configured either -- fine as long as the
    /// session Secret already has a valid refresh token (established via
    /// a prior RefreshSteamAuth call); if it doesn't either, this process
    /// starts with no Steam session at all until RefreshSteamAuth is
    /// called.
    None,
}

pub struct Config {
    pub steam_auth: SteamAuthConfig,
    /// Root of the golden, continuously-synced content tree (server depots
    /// under it directly, workshop mods under `workshop/`). Every
    /// ArmaServer's own content is a read-only btrfs snapshot of this,
    /// taken by services/magpie-csi when its PVC is created -- not this
    /// process's concern at all.
    pub content_root: PathBuf,
    pub listen_addr: String,
    pub pool_size: usize,
    /// Chunk-level concurrency within each depot/mod's own verify+download
    /// pass -- see steam::DEFAULT_DOWNLOAD_WORKERS' own doc for why this
    /// is configurable rather than a hardcoded const (it's the CPU-bound
    /// half of this process' concurrency, unlike SYNC_CONCURRENCY).
    pub download_workers: usize,
    /// How often the ModSource reconciler re-resolves an already-`Synced`
    /// source's candidate IDs on its own periodic requeue, independent of
    /// anything re-registering it -- catches upstream collection-membership
    /// drift even for a source nothing is actively touching right now.
    pub poll_interval_secs: u64,
    /// How often to passively re-run a full `sync_content` pass (server/
    /// CDLC depots plus every registered source's mods) on a timer, on
    /// top of the explicit triggers (`SyncContent` RPC, a server
    /// starting, a ModSource's first resolve). Catches upstream depot/mod
    /// updates for content nothing has explicitly touched in a while --
    /// `0` disables this timer entirely (every sync stays purely
    /// trigger-driven, the behavior before this existed).
    pub content_sync_interval_secs: u64,
    /// Namespace the ModSource reconciler watches, and where the session
    /// Secret (see `secrets.rs`) lives.
    pub namespace: String,
    /// Name of an existing Secret (pre-created by the chart, never by
    /// this process -- see secrets.rs's own doc for why) holding
    /// `steam_user`/`refresh_token` keys once a session has been
    /// established via RefreshSteamAuth.
    pub steam_session_secret_name: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let steam_auth = if bool_env("ANONYMOUS_LOGIN", false) {
            SteamAuthConfig::Anonymous
        } else {
            SteamAuthConfig::None
        };

        Ok(Self {
            steam_auth,
            content_root: env::var("CONTENT_ROOT")
                .unwrap_or_else(|_| "/content".into())
                .into(),
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            pool_size: env::var("POOL_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4),
            download_workers: env::var("DOWNLOAD_WORKERS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(steam_sync::steam::DEFAULT_DOWNLOAD_WORKERS),
            poll_interval_secs: env::var("POLL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1800),
            content_sync_interval_secs: env::var("CONTENT_SYNC_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                // 6h -- frequent enough to catch depot/mod updates within
                // a normal play session's own timescale, infrequent
                // enough not to hammer Steam or waste bandwidth
                // re-verifying content that rarely actually changes.
                .unwrap_or(21600),
            namespace: env::var("NAMESPACE").unwrap_or_else(|_| "default".into()),
            steam_session_secret_name: env::var("STEAM_SESSION_SECRET_NAME")
                .unwrap_or_else(|_| "arma-steam-session".into()),
        })
    }
}

fn bool_env(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(v) => v.eq_ignore_ascii_case("true"),
        Err(_) => default,
    }
}
