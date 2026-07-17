use std::env;

pub struct Config {
    /// Namespace this reconciler manages `ArmaServer` resources and
    /// Deployments in.
    pub namespace: String,
    pub launcher_image: String,
    /// Base URL of sync-daemon's Connect API.
    pub sync_daemon_url: String,
    /// Host directory (hostPath, not a PVC -- see the chart's values.yaml
    /// for why: reflink claims need `content`/`claims` on the very same
    /// real filesystem, which only a direct hostPath pass-through
    /// guarantees on this project's single-node k3s target) sync-daemon
    /// writes claims to, bind-mounted at the same in-container path
    /// (matching sync-daemon's own `CLAIMS_ROOT`) into every launcher Pod
    /// -- the claim path `Claim` returns is an absolute path under that
    /// shared tree, so it's directly usable as `CLAIM_PATH` unmodified.
    pub claims_host_path: String,
    pub claims_root: String,
    /// Host directory holding each server's own operator-provided content
    /// (configs/profiles/keys) -- `<server_root_base>/<ArmaServer name>` is
    /// bind-mounted into that server's Pod at `/arma3/server`. Not
    /// Steam-synced, not tied to a claim's lifecycle, same split the
    /// launcher itself already makes between `CLAIM_PATH` and `SERVER_ROOT`.
    pub server_root_base: String,
    /// Root of the shared volume holding local (zip-uploaded) mods and
    /// missions -- owned/written by services/registry; mounted here
    /// read-only only to resolve local mod paths into launcher Pod specs
    /// (see `reconcile.rs::resolve_mod_paths`), and read-only again in
    /// every launcher Pod itself.
    pub local_content_root: String,
    /// Host directory backing `local_content_root` -- hostPath, same
    /// reasoning as `claims_host_path` (single-node target, no need for a
    /// real RWX-capable storage class when every pod is on the same node
    /// anyway). Bind-mounted by path into every launcher Pod this
    /// reconciler creates.
    pub local_content_host_path: String,
    /// Read-only access to registry's mod_sources table, to resolve a
    /// source ID's kind/reference when building a server's -mod= args.
    /// Shares the cluster's single Postgres instance with
    /// services/registry and services/server-api.
    pub database_url: String,
    /// Names of existing docker-registry Secrets to attach to every
    /// launcher Pod this reconciler creates, so it can pull
    /// `launcher_image` from a private registry -- separate from the
    /// chart's own `imagePullSecrets` support (which only covers the
    /// Deployments Helm templates directly; this reconciler builds the
    /// launcher Pod spec itself, in Rust, bypassing those templates
    /// entirely, so it needs the same list wired in here too).
    pub image_pull_secrets: Vec<String>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            namespace: env::var("NAMESPACE").unwrap_or_else(|_| "default".into()),
            launcher_image: env::var("LAUNCHER_IMAGE").unwrap_or_else(|_| "arma3-launcher:latest".into()),
            sync_daemon_url: env::var("SYNC_DAEMON_URL").unwrap_or_else(|_| "http://sync-daemon:8080".into()),
            claims_host_path: env::var("CLAIMS_HOST_PATH").unwrap_or_else(|_| "/var/lib/magpie/claims".into()),
            claims_root: env::var("CLAIMS_ROOT").unwrap_or_else(|_| "/claims".into()),
            server_root_base: env::var("SERVER_ROOT_BASE").unwrap_or_else(|_| "/srv/arma-servers".into()),
            local_content_root: env::var("LOCAL_CONTENT_ROOT").unwrap_or_else(|_| "/local-content".into()),
            local_content_host_path: env::var("LOCAL_CONTENT_HOST_PATH")
                .unwrap_or_else(|_| "/var/lib/magpie/local-content".into()),
            database_url: require_env("DATABASE_URL")?,
            image_pull_secrets: env::var("IMAGE_PULL_SECRETS")
                .ok()
                .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
                .unwrap_or_default(),
        })
    }
}

fn require_env(key: &str) -> anyhow::Result<String> {
    env::var(key).map_err(|_| anyhow::anyhow!("missing required env var {key}"))
}
