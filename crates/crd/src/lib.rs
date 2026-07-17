//! `ArmaServer` custom resource type. Pure data definition -- no reconcile
//! logic here (that's `services/controller`), so this stays a tiny,
//! dependency-light crate that `services/controller` and any future tooling
//! (a `kubectl`-adjacent CLI, tests) can both depend on without pulling in
//! the whole reconciler.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `mod_source_ids` are opaque IDs (UUIDs) into the controller's own
/// Postgres-backed mod source registry (see `ModSourceService` in
/// `proto/controller/v1/controller.proto`) -- each one resolves, via
/// sync-daemon's `RegisterSource`, to a flat list of Steam mod IDs (for
/// `mod`/`collection`-kind sources) or a path on the shared local-content
/// volume (for `local`-kind, zip-uploaded sources). The reconciler unions
/// all of them into this server's `-mod=` launch args.
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "arma.skua.io",
    version = "v1",
    kind = "ArmaServer",
    namespaced,
    status = "ArmaServerStatus",
    shortname = "arma"
)]
pub struct ArmaServerSpec {
    #[serde(default)]
    pub mod_source_ids: Vec<String>,
    /// Base game port. Arma binds a 5-port range starting here
    /// (`port..=port+4`, matching the launcher image's `EXPOSE 2302-2306`)
    /// -- `ServerService::CreateServer`/`StartServer` reject a port whose
    /// range overlaps any other server that's running or intends to be.
    pub port: u16,
    #[serde(default)]
    pub cdlc: Vec<String>,
    #[serde(default)]
    pub profiling: bool,
    /// Extra launch args, appended verbatim after the generated `-mod=`/CDLC
    /// args.
    #[serde(default)]
    pub params: Vec<String>,
    /// Servers are deployment-like: this is the "should it be up" knob
    /// (`ServerService::StartServer`/`StopServer`/`CreateServer`), separate
    /// from `status.phase`, which tracks how far the reconciler has
    /// actually gotten toward that.
    #[serde(default)]
    pub desired_state: DesiredState,
    /// Filename (under this server's `SERVER_ROOT/configs/`) passed as
    /// `-config=` -- the mission-cycle/security config.
    #[serde(default = "default_arma_config")]
    pub arma_config: String,
    /// Filename (under this server's `SERVER_ROOT/configs/`) passed as
    /// `-cfg=` -- the network performance config.
    #[serde(default = "default_network_config")]
    pub network_config: String,
}

pub fn default_arma_config() -> String {
    "main.cfg".to_string()
}

pub fn default_network_config() -> String {
    "basic.cfg".to_string()
}

/// The 5-port range Arma actually binds for a given base `port` (matching
/// the launcher image's `EXPOSE 2302-2306` for the conventional 2302
/// base) -- game port plus Steam query/voice ports.
pub fn port_range(port: u16) -> std::ops::RangeInclusive<u16> {
    port..=port.saturating_add(4)
}

pub fn port_ranges_overlap(a: u16, b: u16) -> bool {
    let (a_lo, a_hi) = (*port_range(a).start(), *port_range(a).end());
    let (b_lo, b_hi) = (*port_range(b).start(), *port_range(b).end());
    a_lo <= b_hi && b_lo <= a_hi
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum DesiredState {
    #[default]
    Running,
    Stopped,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ArmaServerStatus {
    #[serde(default)]
    pub phase: ArmaServerPhase,
    /// The claim path returned by sync-daemon's last successful `Claim`,
    /// mounted into the launcher Pod as `CLAIM_PATH`. Empty until the first
    /// claim completes.
    #[serde(default)]
    pub claim_path: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum ArmaServerPhase {
    /// `desired_state` is `Stopped` and no instance is running. Also the
    /// implicit starting point conceptually, though new servers default to
    /// `desired_state: Running` and so normally move past this immediately.
    #[default]
    Stopped,
    Pending,
    Claiming,
    Running,
    Failed,
}
