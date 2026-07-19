use std::env;

pub struct Config {
    /// Namespace this reconciler manages `ArmaServer` resources and
    /// Deployments in.
    pub namespace: String,
    pub launcher_image: String,
    /// Base URL of sync-daemon's Connect API.
    pub sync_daemon_url: String,
    /// PersistentVolumeClaim (magpie-reflink StorageClass, see
    /// magpie-csi) sync-daemon writes claims to, mounted read-only (via
    /// its `claims` subPath -- `content` is the other) into every
    /// launcher Pod at the same in-container path (matching
    /// sync-daemon's own `CLAIMS_ROOT`) -- the claim path `Claim`
    /// returns is an absolute path under that shared tree, so it's
    /// directly usable as `CLAIM_PATH` unmodified. Same PVC sync-daemon
    /// itself mounts, so `content`/`claims` always land on the very same
    /// real filesystem (required for `cp --reflink=always` to CoW at
    /// all) without either side having to reason about it.
    pub reflink_pvc_name: String,
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
    /// Names of existing docker-registry Secrets to attach to every
    /// launcher Pod this reconciler creates, so it can pull
    /// `launcher_image` from a private registry -- separate from the
    /// chart's own `imagePullSecrets` support (which only covers the
    /// Deployments Helm templates directly; this reconciler builds the
    /// launcher Pod spec itself, in Rust, bypassing those templates
    /// entirely, so it needs the same list wired in here too).
    pub image_pull_secrets: Vec<String>,
    /// Admin-level connection to the cluster's shared Postgres -- used
    /// only by `postgres_bootstrap` to provision `app_postgres_role` on
    /// startup. Never touched by reconcile.rs itself.
    pub database_url: String,
    /// Role name for the technical Postgres user launcher's own Arma
    /// process connects as (distinct from whatever role `database_url`
    /// authenticates as). See `postgres_bootstrap`.
    pub app_postgres_role: String,
    /// Secret (chart-created, see app-postgres-secret.yaml) holding
    /// `app_postgres_role`'s password under a `POSTGRES_PASSWORD` key --
    /// read once on startup to provision the role, and referenced again
    /// per-launcher-Pod as a `secretKeyRef` for `DATABASE_PASSWORD`.
    pub app_postgres_secret_name: String,
    pub app_postgres_database: String,
    /// Passed straight through into every launcher Pod's own
    /// `DATABASE_HOST`/`DATABASE_PORT` env vars.
    pub app_postgres_host: String,
    pub app_postgres_port: String,
    /// Chart-managed ConfigMap holding the cluster-wide baseline Arma
    /// config every server's main.cfg/basic.cfg starts from -- see
    /// arma_config.rs. A given `ArmaServer`'s own `spec.config_map`, if
    /// set, layers per-server overrides on top of this.
    pub arma_config_baseline: String,
    /// Namespace `{{secret:<name>/<key>}}` references (arma_config.rs)
    /// resolve against -- deliberately *not* this reconciler's own
    /// namespace, which also holds arma-postgres-creds/ghcr-pull-secret/
    /// etc. An operator-controlled ConfigMap value naming a Secret to
    /// read is effectively an exfiltration primitive if it can reach any
    /// secret in the platform's own namespace; scoping it to a separate,
    /// dedicated namespace (chart-created, see user-secrets-namespace.yaml)
    /// means the worst a malicious/mistaken ConfigMap value can do is
    /// read a secret an operator deliberately put there for this purpose.
    pub user_secrets_namespace: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            namespace: env::var("NAMESPACE").unwrap_or_else(|_| "default".into()),
            launcher_image: env::var("LAUNCHER_IMAGE")
                .unwrap_or_else(|_| "arma3-launcher:latest".into()),
            sync_daemon_url: env::var("SYNC_DAEMON_URL")
                .unwrap_or_else(|_| "http://sync-daemon:8080".into()),
            reflink_pvc_name: env::var("REFLINK_PVC_NAME")
                .unwrap_or_else(|_| "magpie-reflink-content".into()),
            claims_root: env::var("CLAIMS_ROOT").unwrap_or_else(|_| "/claims".into()),
            server_root_base: env::var("SERVER_ROOT_BASE")
                .unwrap_or_else(|_| "/srv/arma-servers".into()),
            local_content_root: env::var("LOCAL_CONTENT_ROOT")
                .unwrap_or_else(|_| "/local-content".into()),
            local_content_host_path: env::var("LOCAL_CONTENT_HOST_PATH")
                .unwrap_or_else(|_| "/var/lib/magpie/local-content".into()),
            image_pull_secrets: env::var("IMAGE_PULL_SECRETS")
                .ok()
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
            database_url: env::var("DATABASE_URL").unwrap_or_default(),
            app_postgres_role: env::var("APP_POSTGRES_ROLE").unwrap_or_else(|_| "arma_app".into()),
            app_postgres_secret_name: env::var("APP_POSTGRES_SECRET_NAME")
                .unwrap_or_else(|_| "arma-app-postgres-creds".into()),
            app_postgres_database: env::var("APP_POSTGRES_DATABASE")
                .unwrap_or_else(|_| "arma".into()),
            app_postgres_host: env::var("APP_POSTGRES_HOST").unwrap_or_default(),
            app_postgres_port: env::var("APP_POSTGRES_PORT").unwrap_or_else(|_| "5432".into()),
            arma_config_baseline: env::var("ARMA_CONFIG_BASELINE_CONFIGMAP")
                .unwrap_or_else(|_| "arma-config-baseline".into()),
            user_secrets_namespace: env::var("USER_SECRETS_NAMESPACE")
                .unwrap_or_else(|_| "magpie-user-secrets".into()),
        })
    }
}
