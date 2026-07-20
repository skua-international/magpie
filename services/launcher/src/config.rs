use std::env;
use std::path::PathBuf;

pub struct Config {
    /// Root of the Steam-synced content this run should use -- the arma3
    /// binary itself and workshop/ mods, produced by sync-daemon's Claim()
    /// (a read-only btrfs snapshot of its golden tree) and bind-mounted
    /// in here.
    /// Everything else (configs/, keys/, profiles/) stays under the
    /// separate, fixed SERVER_ROOT -- that content is operator-provided,
    /// not Steam-synced, and has no reason to be tied to a claim's
    /// lifecycle.
    pub claim_path: PathBuf,
    /// Workshop mods this specific server should load, as `-mod=`-ready
    /// path strings (`workshop/<id>`) relative to `claim_path`. In the
    /// full design this comes from the controller (built from
    /// RegisterSource's resolved mod list); for now it's read directly
    /// from a `MODS` env var (semicolon-separated) for manual testing.
    pub mods: Vec<String>,
    pub arma_binary: String,
    pub arma_cdlc: Vec<String>,
    /// `None` if `SYNC_DAEMON_URL` is unset -- delete-claim-on-exit (see
    /// launch.rs) just gets skipped with a warning rather than refusing
    /// to launch the actual game server over a missing side-effect. Only
    /// unset in practice for manual/local testing outside a real
    /// controller-managed Deployment (which always sets it).
    pub sync_daemon_url: Option<String>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            claim_path: require_env("CLAIM_PATH")?.into(),
            mods: env::var("MODS")
                .unwrap_or_default()
                .split(';')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            arma_binary: env::var("ARMA_BINARY").unwrap_or_else(|_| "./arma3server_x64".into()),
            arma_cdlc: env::var("ARMA_CDLC")
                .unwrap_or_default()
                .split(';')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            sync_daemon_url: env::var("SYNC_DAEMON_URL").ok(),
        })
    }
}

fn require_env(key: &str) -> anyhow::Result<String> {
    env::var(key).map_err(|_| anyhow::anyhow!("missing required env var {key}"))
}
