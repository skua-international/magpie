use std::env;

use authn::jwt::JwtConfig;

pub struct Config {
    /// Namespace whose Secrets `AdminService`'s secret RPCs manage.
    ///
    /// Deliberately a distinct namespace from `namespace`: that one holds
    /// Postgres credentials and image pull secrets, and keeping
    /// operator-managed secrets out of it is the whole reason
    /// charts/magpie/templates/user-secrets-namespace.yaml exists. The
    /// RBAC granted for these RPCs is scoped to this namespace alone.
    pub user_secrets_namespace: String,
    /// Root of the shared volume holding local (zip-uploaded) mods and
    /// missions -- read-write here (this service is the sole writer),
    /// read-only in every launcher Pod the controller's reconciler creates
    /// (must be the same path there, since paths recorded here are handed
    /// back to the controller/launcher verbatim).
    pub local_content_root: String,
    pub listen_addr: String,
    pub database_url: String,
    pub sync_daemon_url: String,
    /// Namespace `ModSource` objects are created/read/deleted in.
    pub namespace: String,
    /// Namespace `ArmaServer` objects and ConfigMaps are read/written in
    /// by AdminService's ExportState/ImportState (service/admin.rs) --
    /// kept distinct from `namespace` above because it has to match
    /// services/controller's own namespace (`controller.namespace`/
    /// `serverApi.namespace` in values.yaml), which *can* diverge from
    /// the release namespace `namespace` always uses, if ever explicitly
    /// overridden away from its documented default. Falls back to
    /// `namespace` when unset, matching that default case.
    pub armaserver_namespace: String,
    /// Name of the cluster-wide Arma config baseline ConfigMap --
    /// AdminService's ExportState always includes this one regardless of
    /// whether any server references a per-server override, so it needs
    /// the same name services/controller was handed at deploy time
    /// (`ARMA_CONFIG_BASELINE_CONFIGMAP`, see charts/magpie/templates/
    /// arma-config-baseline-configmap.yaml for the chart's own naming).
    pub arma_config_baseline: String,
    pub jwt: JwtConfig,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let namespace = env::var("NAMESPACE").unwrap_or_else(|_| "default".into());
        let armaserver_namespace =
            env::var("ARMASERVER_NAMESPACE").unwrap_or_else(|_| namespace.clone());
        Ok(Self {
            user_secrets_namespace: env::var("USER_SECRETS_NAMESPACE")
                .unwrap_or_else(|_| "magpie-user-secrets".into()),
            local_content_root: env::var("LOCAL_CONTENT_ROOT")
                .unwrap_or_else(|_| "/local-content".into()),
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8444".into()),
            database_url: require_env("DATABASE_URL")?,
            sync_daemon_url: env::var("SYNC_DAEMON_URL")
                .unwrap_or_else(|_| "http://sync-daemon:8080".into()),
            namespace,
            armaserver_namespace,
            arma_config_baseline: require_env("ARMA_CONFIG_BASELINE_CONFIGMAP")?,
            jwt: JwtConfig {
                jwks_url: require_env("JWKS_URL")?,
                issuer: require_env("JWT_ISSUER")?,
                audience: require_env("JWT_AUDIENCE")?,
            },
        })
    }
}

fn require_env(key: &str) -> anyhow::Result<String> {
    env::var(key).map_err(|_| anyhow::anyhow!("missing required env var {key}"))
}
