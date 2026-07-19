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
    /// Name of an operator-created ConfigMap (same namespace) providing
    /// per-server overrides on top of the cluster's baseline Arma config
    /// (see `services/controller/src/arma_config.rs`). Unset means
    /// "baseline only".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map: Option<String>,
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

/// Registers and tracks a *source* of mods -- a single Workshop mod, a
/// Workshop collection, a preset HTML export, or a locally-uploaded zip --
/// independent of any `ArmaServer`'s lifecycle. An `ArmaServer` references
/// zero or more of these by name (`mod_source_ids`); deleting a server does
/// not delete the sources it referenced (content may be worth keeping
/// synced/stored with nothing currently using it).
///
/// Reconciled by `services/sync-daemon` for every kind except `Local`,
/// which registry resolves and marks `Synced` synchronously at creation
/// time (extracting a zip needs no Steam session, so there's nothing to
/// reconcile). Creating this only resolves what a source *means* (a mod ID,
/// a collection's expanded members, ...) -- it does not itself trigger a
/// download; that happens when something actually claims the content (an
/// `ArmaServer` starting, or an explicit force-sync).
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "arma.skua.io",
    version = "v1",
    kind = "ModSource",
    namespaced,
    status = "ModSourceStatus",
    shortname = "modsrc",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Kind","type":"string","jsonPath":".status.kind"}"#,
    printcolumn = r#"{"name":"Size","type":"integer","jsonPath":".status.size_bytes"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
pub struct ModSourceSpec {
    pub source: ModSourceInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ModSourceInput {
    /// A single raw Steam Workshop URL -- a mod or a collection,
    /// indistinguishable by shape alone. Resolved via sync-daemon's
    /// authenticated Steam session, which is also what correctly handles
    /// unlisted/private content a plain HTTP call can't see.
    SteamUrl(String),
    /// A URL to a preset HTML export -- fetched and scanned for
    /// `filedetails/?id=` links.
    HtmlUrl(String),
    /// The preset HTML export's content itself, already in hand -- scanned
    /// directly, no fetch. Preset exports are small (tens of KB), nowhere
    /// near etcd's per-object size limit.
    HtmlContent(String),
    /// A caller-assigned identifier, unique among all local mods -- the
    /// zip itself is already extracted (by registry, before this object is
    /// created) under this ID as its on-disk directory name; this is just
    /// a stable reference to it, never the zip bytes themselves.
    Local { unique_id: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ModSourceStatus {
    #[serde(default)]
    pub phase: ModSourcePhase,
    /// "mod" | "collection" | "local" | "preset" -- empty until resolved.
    /// A plain string, not a second enum, purely for display/listing; nothing
    /// reconciles against it (see `ModSourceInput` for the enum that actually
    /// drives behavior).
    #[serde(default)]
    pub kind: String,
    /// Workshop display name (mod/collection), empty for local and
    /// multi-mod preset sources where no single title applies.
    #[serde(default)]
    pub display_name: String,
    /// This source's fully-resolved, flat mod ID list (collections
    /// expanded) -- empty for local sources.
    #[serde(default)]
    pub resolved_mod_ids: Vec<u64>,
    /// Sum of on-disk size across every mod this source currently resolves
    /// to (local: the extracted zip's own directory size). 0 until synced
    /// at least once.
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum ModSourcePhase {
    #[default]
    Pending,
    Resolving,
    Synced,
    Failed,
}
