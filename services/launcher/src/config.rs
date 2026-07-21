use std::env;
use std::path::PathBuf;

pub struct Config {
    /// Root of the Steam-synced content this run should use -- the arma3
    /// binary itself and workshop/ mods. A fixed path (services/controller's
    /// own CLAIM_PATH constant): the CSI inline ephemeral volume mounted
    /// there (see reconcile.rs's launcher Pod spec) is a fresh read-only
    /// btrfs snapshot of sync-daemon's golden content tree, provisioned
    /// by services/magpie-csi the moment this Pod is scheduled -- there's
    /// no per-launch job/claim ID to embed in the path anymore, and
    /// nothing in this process's own lifecycle (including on exit) needs
    /// to release it; that volume's lifecycle is owned by the Pod itself.
    /// Everything else (configs/, keys/, profiles/) stays under the
    /// separate, fixed SERVER_ROOT -- that content is operator-provided,
    /// not Steam-synced, and has no reason to be tied to this volume's
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
    /// Set only on a headless-client launcher Pod (see
    /// `services/controller/src/reconcile.rs`'s `ensure_hc_deployment`) --
    /// presence alone is what switches `launch::run` into client mode at
    /// all, not a separate bool env var, since a connect target is
    /// required for `-client` to mean anything regardless.
    pub client_connect: Option<String>,
    /// Only ever set alongside `client_connect`, and only when the owning
    /// server's own `password` is non-empty -- a headless client is just
    /// another connecting client as far as Arma's `password[]` check is
    /// concerned.
    pub client_password: Option<String>,
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
            client_connect: env::var("ARMA_CLIENT_CONNECT")
                .ok()
                .filter(|s| !s.is_empty()),
            client_password: env::var("ARMA_SERVER_PASSWORD")
                .ok()
                .filter(|s| !s.is_empty()),
        })
    }
}

fn require_env(key: &str) -> anyhow::Result<String> {
    env::var(key).map_err(|_| anyhow::anyhow!("missing required env var {key}"))
}
