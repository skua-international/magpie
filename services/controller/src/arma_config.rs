//! Renders main.cfg/basic.cfg from a cluster-wide baseline ConfigMap
//! (chart-managed, see charts/magpie/templates/arma-config-baseline-
//! configmap.yaml) merged with an optional per-server override ConfigMap
//! (`ArmaServer.spec.config_map`, per-server wins key-by-key), then
//! writes the results to disk under this server's `SERVER_ROOT/configs/`
//! -- plain files, not a mounted ConfigMap volume, so they stay
//! hand-editable in place afterward.
//!
//! ConfigMap `data` is flat string->string, so list-valued fields
//! (`missions_whitelist`, `motd`) are comma-separated strings, and
//! bool/number fields are their literal string forms (`"true"`/`"64"`).
//!
//! # Placeholders
//! `hostname`, `password`, `password_admin`, `server_command_password`,
//! and each `motd` entry may contain:
//! - `{{server_name}}` -- this `ArmaServer`'s own object name
//! - `{{prefix}}` / `{{suffix}}` -- the merged config's own `prefix`/
//!   `suffix` keys (sane default hostname: `"{{prefix}}{{server_name}}{{suffix}}"`)
//! - `{{secret:<name>/<key>}}` (password-family fields only) -- resolved
//!   via a live Secret lookup in the same namespace, instead of the
//!   value living in the ConfigMap directly
//!
//! `admins[]`/`filePatchingExceptions[]` are never ConfigMap keys --
//! computed every call from identities holding the `arma:admin`/
//! `arma:filepatch` scopes (see `registry_db::steam_ids_with_scope`).

use std::collections::BTreeMap;

use anyhow::Context;
use crd::ArmaServer;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::Api;
use kube::{Client, ResourceExt};
use sqlx::PgPool;

pub async fn render_and_write(
    client: &Client,
    pool: &PgPool,
    namespace: &str,
    user_secrets_namespace: &str,
    server_root_base: &str,
    baseline_configmap: &str,
    obj: &ArmaServer,
) -> anyhow::Result<Vec<(String, String)>> {
    let name = obj.name_any();
    let merged = fetch_and_merge(
        client,
        namespace,
        baseline_configmap,
        obj.spec.config_map.as_deref(),
    )
    .await?;

    let admins = registry_db::steam_ids_with_scope(pool, "arma:admin").await?;
    let filepatch = registry_db::steam_ids_with_scope(pool, "arma:filepatch").await?;

    let main = resolve_main_config(
        client,
        user_secrets_namespace,
        &merged,
        &name,
        admins,
        filepatch,
        obj.spec.headless_clients > 0,
    )
    .await?;
    let basic = resolve_basic_config(&merged);

    // ARMA_LIMITFPS always present (limit_fps has a real default, unlike
    // most other fields here), ARMA_PARAMS only when additional_params is
    // actually set -- launch.rs's own read of it is optional. Ahead of
    // extract_env_vars's own output below, not after: that's the
    // deliberate last-word override (an operator setting env.ARMA_PARAMS
    // directly still wins over additional_params), same precedent as
    // every other fixed env var already established.
    let (limit_fps, additional_params) = resolve_launch_params(&merged);
    let mut extra_env = vec![("ARMA_LIMITFPS".to_string(), limit_fps.to_string())];
    if !additional_params.is_empty() {
        extra_env.push(("ARMA_PARAMS".to_string(), additional_params));
    }
    extra_env.extend(extract_env_vars(client, user_secrets_namespace, &merged, &name).await?);

    let dir = format!("{server_root_base}/{name}/configs");
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("failed to create {dir}"))?;
    tokio::fs::write(format!("{dir}/main.cfg"), render_main_cfg(&main)).await?;
    tokio::fs::write(format!("{dir}/basic.cfg"), render_basic_cfg(&basic)).await?;
    Ok(extra_env)
}

/// `-limitFPS=`/extra launch args -- deliberately not part of
/// `ResolvedMainConfig`: neither is actual main.cfg *file content*
/// (`render_main_cfg` never touches them), they're launcher argv values,
/// resolved from the exact same merged ConfigMap as everything else here
/// purely because that's the one place "configure this server" already
/// means, not because they belong next to hostname/password/etc.
/// `limit_fps` always has a value (default 300); `additional_params` is
/// `""` when unset, same as every other raw-passthrough field.
fn resolve_launch_params(m: &BTreeMap<String, String>) -> (i64, String) {
    let limit_fps = parse_num(m, "limit_fps").unwrap_or(300);
    let additional_params = m.get("additional_params").cloned().unwrap_or_default();
    (limit_fps, additional_params)
}

/// What a `HeadlessClient` needs from the owning server's own merged
/// config -- resolved independently from `render_and_write` (not passed
/// through from the main server's own last reconcile pass) so
/// `services/controller/src/reconcile.rs`'s `hc_apply` stays correct
/// even if it runs out of step with the ArmaServer's own reconcile (e.g.
/// right after a controller restart). A headless client is just another
/// connecting client as far as `password[]`/launch flags are concerned,
/// so it needs the exact same resolved values the main server's own
/// config was (or will be) written with.
pub struct HeadlessClientLaunchConfig {
    pub password: String,
    pub limit_fps: i64,
    pub additional_params: String,
}

pub async fn resolve_headless_client_config(
    client: &Client,
    namespace: &str,
    user_secrets_namespace: &str,
    baseline_configmap: &str,
    obj: &ArmaServer,
) -> anyhow::Result<HeadlessClientLaunchConfig> {
    let merged = fetch_and_merge(
        client,
        namespace,
        baseline_configmap,
        obj.spec.config_map.as_deref(),
    )
    .await?;
    let prefix = merged.get("prefix").cloned().unwrap_or_default();
    let suffix = merged
        .get("suffix")
        .cloned()
        .unwrap_or_else(|| " | Powered by MAGPIE".to_string());
    let raw_password = substitute_simple(
        merged.get("password").map(String::as_str).unwrap_or(""),
        &obj.name_any(),
        &prefix,
        &suffix,
    );
    let password = resolve_secret_ref(client, user_secrets_namespace, &raw_password).await?;
    let (limit_fps, additional_params) = resolve_launch_params(&merged);
    Ok(HeadlessClientLaunchConfig {
        password,
        limit_fps,
        additional_params,
    })
}

/// Any merged-config key of the form `env.<NAME>` becomes an extra
/// `<NAME>` env var on the launcher container -- lets an operator inject
/// arbitrary launcher-consumed env vars through the same ConfigMap
/// mechanism as everything else here, with the same placeholder and
/// `{{secret:<name>/<key>}}` substitution support as password-family
/// main.cfg fields. E.g. `env.SOME_API_KEY = "{{secret:my-secret/key}}"`
/// -> the launcher Pod gets `SOME_API_KEY=<resolved value>`.
async fn extract_env_vars(
    client: &Client,
    user_secrets_namespace: &str,
    m: &BTreeMap<String, String>,
    server_name: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let prefix = m.get("prefix").cloned().unwrap_or_default();
    let suffix = m
        .get("suffix")
        .cloned()
        .unwrap_or_else(|| " | Powered by MAGPIE".to_string());

    let mut out = Vec::new();
    for (key, raw_value) in m {
        let Some(env_name) = key.strip_prefix("env.") else {
            continue;
        };
        let substituted = substitute_simple(raw_value, server_name, &prefix, &suffix);
        let resolved = resolve_secret_ref(client, user_secrets_namespace, &substituted).await?;
        out.push((env_name.to_string(), resolved));
    }
    Ok(out)
}

async fn fetch_and_merge(
    client: &Client,
    namespace: &str,
    baseline_name: &str,
    override_name: Option<&str>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let mut merged: BTreeMap<String, String> = api
        .get(baseline_name)
        .await
        .with_context(|| format!("failed to fetch baseline ConfigMap {baseline_name}"))?
        .data
        .unwrap_or_default()
        .into_iter()
        .collect();
    if let Some(name) = override_name {
        let over = api
            .get(name)
            .await
            .with_context(|| format!("failed to fetch per-server ConfigMap {name}"))?
            .data
            .unwrap_or_default();
        merged.extend(over);
    }
    Ok(merged)
}

/// `{{secret:<name>/<key>}}`, resolved via a live lookup against
/// `user_secrets_namespace` -- deliberately never the `ArmaServer`'s own
/// namespace (see `Config::user_secrets_namespace`'s doc for why: an
/// operator-controlled ConfigMap value naming a Secret to read would
/// otherwise be able to reach arma-postgres-creds/ghcr-pull-secret/etc.
/// alongside it). Anything else (including an empty string) passes
/// through unchanged.
async fn resolve_secret_ref(
    client: &Client,
    user_secrets_namespace: &str,
    value: &str,
) -> anyhow::Result<String> {
    let Some(inner) = value
        .strip_prefix("{{secret:")
        .and_then(|s| s.strip_suffix("}}"))
    else {
        return Ok(value.to_string());
    };
    let (name, key) = inner
        .split_once('/')
        .with_context(|| format!("malformed {{{{secret:...}}}} reference: {value}"))?;
    let api: Api<Secret> = Api::namespaced(client.clone(), user_secrets_namespace);
    let secret = api.get(name).await.with_context(|| {
        format!("failed to fetch Secret {name} for {{{{secret:...}}}} reference")
    })?;
    let bytes = secret
        .data
        .as_ref()
        .and_then(|d| d.get(key))
        .with_context(|| format!("Secret {name} has no key {key}"))?;
    Ok(String::from_utf8(bytes.0.clone())?)
}

fn substitute_simple(value: &str, server_name: &str, prefix: &str, suffix: &str) -> String {
    value
        .replace("{{server_name}}", server_name)
        .replace("{{prefix}}", prefix)
        .replace("{{suffix}}", suffix)
}

fn parse_bool(m: &BTreeMap<String, String>, key: &str) -> Option<bool> {
    m.get(key).and_then(|v| v.trim().parse::<bool>().ok())
}

fn parse_num(m: &BTreeMap<String, String>, key: &str) -> Option<i64> {
    m.get(key).and_then(|v| v.trim().parse::<i64>().ok())
}

fn parse_float(m: &BTreeMap<String, String>, key: &str) -> Option<f64> {
    m.get(key).and_then(|v| v.trim().parse::<f64>().ok())
}

fn parse_list(m: &BTreeMap<String, String>, key: &str) -> Vec<String> {
    m.get(key)
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `"<level>:<seconds>,<level>:<seconds>,..."` -- same comma-separated
/// convention as every other list-valued key here, just with a `:` pair
/// separator since kickTimeout[] entries are two numbers, not one. `None`
/// only when the key is absent entirely (vs. present-but-empty, which is
/// `Some(vec![])`), so callers can tell "not configured, use the actual
/// default" apart from "explicitly cleared."
fn parse_pairs(m: &BTreeMap<String, String>, key: &str) -> Option<Vec<(i64, i64)>> {
    let raw = m.get(key)?;
    Some(
        raw.split(',')
            .filter_map(|pair| {
                let (a, b) = pair.trim().split_once(':')?;
                Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
            })
            .collect(),
    )
}

pub struct ResolvedMainConfig {
    pub hostname: String,
    pub max_players: i64,
    pub force_difficulty: bool,
    pub forced_difficulty: String,
    pub missions_whitelist: Vec<String>,
    pub persist_without_players: bool,
    pub use_battleye: bool,
    pub verify_signatures: bool,
    pub skip_lobby: bool,
    pub allow_zeus_composition_scripts: bool,
    pub allow_custom_glasses: bool,
    pub max_ping: Option<i64>,
    pub max_packet_loss: Option<i64>,
    pub max_desync: Option<i64>,
    pub password_admin: String,
    pub password: String,
    pub server_command_password: String,
    pub motd: Vec<String>,
    pub motd_interval: Option<i64>,
    pub other_properties: String,
    pub admins: Vec<String>,
    pub file_patching_exceptions: Vec<String>,
    /// `(level, seconds)` pairs -- old-fleet default (see issue #25) is
    /// `{0,1},{1,1},{2,1},{3,1}`, applied whenever the key is absent
    /// entirely (an explicit empty list stays empty, same as every other
    /// list field here).
    pub kick_timeout: Vec<(i64, i64)>,
    /// 0/1/2, matching Arma's own allowedFilePatching values directly
    /// (not a bool) -- old-fleet default is 1.
    pub allowed_file_patching: i64,
    /// Old-fleet default is 1 (VON disabled).
    pub disable_von: bool,
    /// `spec.headless_clients > 0` -- when true, render_main_cfg adds
    /// `headlessClients[] = {"127.0.0.1"};`/`localClient[] =
    /// {"127.0.0.1"};` (single-node scope: every HC pod is hostNetwork:
    /// true same as the main server, so every HC's own connection really
    /// does come from 127.0.0.1, no per-HC IP discovery needed -- see
    /// `crd::HeadlessClientSpec`'s own doc). No prior ConfigMap-driven
    /// mechanism ever produced these two keys, so there's no existing
    /// operator override to preserve here, unlike every other field in
    /// this struct.
    pub headless_clients: bool,
}

pub struct ResolvedBasicConfig {
    pub max_msg_send: Option<i64>,
    pub max_size_guaranteed: Option<i64>,
    pub max_size_nonguaranteed: Option<i64>,
    /// Bandwidth fields are quoted strings in basic.cfg (e.g. `"100Mbps"`),
    /// not plain numbers -- kept verbatim rather than parsed.
    pub min_bandwidth: Option<String>,
    pub max_bandwidth: Option<String>,
    pub min_error_to_send: Option<f64>,
    pub min_error_to_send_near: Option<f64>,
    pub other_properties: String,
}

async fn resolve_main_config(
    client: &Client,
    namespace: &str,
    m: &BTreeMap<String, String>,
    server_name: &str,
    admins: Vec<String>,
    filepatch: Vec<String>,
    headless_clients: bool,
) -> anyhow::Result<ResolvedMainConfig> {
    let prefix = m.get("prefix").cloned().unwrap_or_default();
    let suffix = m
        .get("suffix")
        .cloned()
        .unwrap_or_else(|| " | Powered by MAGPIE".to_string());
    let subst = |v: &str| substitute_simple(v, server_name, &prefix, &suffix);

    let hostname_raw = m
        .get("hostname")
        .cloned()
        .unwrap_or_else(|| "{{prefix}}{{server_name}}{{suffix}}".to_string());

    let password_admin_raw = subst(m.get("password_admin").map(String::as_str).unwrap_or(""));
    let password_raw = subst(m.get("password").map(String::as_str).unwrap_or(""));
    let server_command_password_raw = subst(
        m.get("server_command_password")
            .map(String::as_str)
            .unwrap_or(""),
    );

    Ok(ResolvedMainConfig {
        hostname: subst(&hostname_raw),
        max_players: parse_num(m, "max_players").unwrap_or(64),
        force_difficulty: parse_bool(m, "force_difficulty").unwrap_or(false),
        forced_difficulty: m
            .get("forced_difficulty")
            .cloned()
            .unwrap_or_else(|| "veteran".to_string()),
        missions_whitelist: parse_list(m, "missions_whitelist"),
        persist_without_players: parse_bool(m, "persist_without_players").unwrap_or(false),
        use_battleye: parse_bool(m, "use_battleEye").unwrap_or(false),
        verify_signatures: parse_bool(m, "verify_signatures").unwrap_or(true),
        skip_lobby: parse_bool(m, "skip_lobby").unwrap_or(false),
        allow_zeus_composition_scripts: parse_bool(m, "allow_zeus_composition_scripts")
            .unwrap_or(true),
        allow_custom_glasses: parse_bool(m, "allow_custom_glasses").unwrap_or(false),
        max_ping: parse_num(m, "max_ping").or(Some(300)),
        max_packet_loss: parse_num(m, "max_packet_loss"),
        max_desync: parse_num(m, "max_desync"),
        password_admin: resolve_secret_ref(client, namespace, &password_admin_raw).await?,
        password: resolve_secret_ref(client, namespace, &password_raw).await?,
        server_command_password: resolve_secret_ref(
            client,
            namespace,
            &server_command_password_raw,
        )
        .await?,
        motd: parse_list(m, "motd")
            .into_iter()
            .map(|s| subst(&s))
            .collect(),
        motd_interval: parse_num(m, "motd_interval"),
        other_properties: m.get("other_properties").cloned().unwrap_or_default(),
        admins,
        file_patching_exceptions: filepatch,
        kick_timeout: parse_pairs(m, "kick_timeout")
            .unwrap_or_else(|| vec![(0, 1), (1, 1), (2, 1), (3, 1)]),
        allowed_file_patching: parse_num(m, "allowed_file_patching").unwrap_or(1),
        disable_von: parse_bool(m, "disable_von").unwrap_or(true),
        headless_clients,
    })
}

fn resolve_basic_config(m: &BTreeMap<String, String>) -> ResolvedBasicConfig {
    ResolvedBasicConfig {
        max_msg_send: parse_num(m, "max_msg_send"),
        max_size_guaranteed: parse_num(m, "max_size_guaranteed"),
        max_size_nonguaranteed: parse_num(m, "max_size_nonguaranteed"),
        min_bandwidth: m.get("min_bandwidth").cloned(),
        max_bandwidth: m.get("max_bandwidth").cloned(),
        min_error_to_send: parse_float(m, "min_error_to_send"),
        min_error_to_send_near: parse_float(m, "min_error_to_send_near"),
        other_properties: m.get("basic_other_properties").cloned().unwrap_or_default(),
    }
}

/// Bohemia config string escaping -- a literal `"` is doubled, same
/// convention `main.cfg`'s own hand-written examples already follow.
fn escape(s: &str) -> String {
    s.replace('"', "\"\"")
}

fn bool_num(b: bool) -> u8 {
    if b { 1 } else { 0 }
}

fn string_array(items: &[String]) -> String {
    if items.is_empty() {
        "{}".to_string()
    } else {
        let quoted: Vec<String> = items.iter().map(|s| format!("\"{}\"", escape(s))).collect();
        format!("{{{}}}", quoted.join(","))
    }
}

/// `{ { a, b }, { a, b }, ... }` -- kickTimeout[]'s own two-number-pair
/// shape, distinct from string_array's single-value entries.
fn pair_array(items: &[(i64, i64)]) -> String {
    if items.is_empty() {
        "{}".to_string()
    } else {
        let entries: Vec<String> = items
            .iter()
            .map(|(level, seconds)| format!("{{ {level}, {seconds} }}"))
            .collect();
        format!("{{{}}}", entries.join(","))
    }
}

pub fn render_main_cfg(cfg: &ResolvedMainConfig) -> String {
    let mut out = String::new();
    out += &format!("hostname = \"{}\";\n\n", escape(&cfg.hostname));
    out += &format!("maxPlayers = {};\n\n", cfg.max_players);
    out += &format!("admins[] = {};\n", string_array(&cfg.admins));
    out += &format!(
        "filePatchingExceptions[] = {};\n\n",
        string_array(&cfg.file_patching_exceptions)
    );
    if cfg.force_difficulty {
        out += &format!(
            "forcedDifficulty = \"{}\";\n",
            escape(&cfg.forced_difficulty)
        );
    }
    out += &format!(
        "missionWhitelist[] = {};\n",
        string_array(&cfg.missions_whitelist)
    );
    out += &format!("persistent = {};\n", bool_num(cfg.persist_without_players));
    out += &format!("BattlEye = {};\n", bool_num(cfg.use_battleye));
    out += &format!(
        "verifySignatures = {};\n",
        if cfg.verify_signatures { 2 } else { 0 }
    );
    out += &format!("skipLobby = {};\n", bool_num(cfg.skip_lobby));
    out += &format!("allowedFilePatching = {};\n", cfg.allowed_file_patching);
    out += &format!("disableVON = {};\n", bool_num(cfg.disable_von));
    out += &format!("kickTimeout[] = {};\n", pair_array(&cfg.kick_timeout));
    out += &format!(
        "zeusCompositionScriptLevel = {};\n",
        if cfg.allow_zeus_composition_scripts {
            2
        } else {
            0
        }
    );
    out += &format!("allowProfileGlasses = {};\n", cfg.allow_custom_glasses);
    if let Some(v) = cfg.max_ping {
        out += &format!("MaxPing = {v};\n");
    }
    if let Some(v) = cfg.max_packet_loss {
        out += &format!("maxPacketLoss = {v};\n");
    }
    if let Some(v) = cfg.max_desync {
        out += &format!("maxDesync = {v};\n");
    }
    out += &format!("passwordAdmin = \"{}\";\n", escape(&cfg.password_admin));
    out += &format!("password = \"{}\";\n", escape(&cfg.password));
    out += &format!(
        "serverCommandPassword = \"{}\";\n",
        escape(&cfg.server_command_password)
    );
    out += &format!("motd[] = {};\n", string_array(&cfg.motd));
    if let Some(v) = cfg.motd_interval {
        out += &format!("motdInterval = {v};\n");
    }
    if cfg.headless_clients {
        out += "headlessClients[] = {\"127.0.0.1\"};\n";
        out += "localClient[] = {\"127.0.0.1\"};\n";
    }
    if !cfg.other_properties.is_empty() {
        out += "\n";
        out += &cfg.other_properties;
        out += "\n";
    }
    out
}

pub fn render_basic_cfg(cfg: &ResolvedBasicConfig) -> String {
    let mut out = String::new();
    if let Some(v) = cfg.max_msg_send {
        out += &format!("MaxMsgSend = {v};\n");
    }
    if let Some(v) = cfg.max_size_guaranteed {
        out += &format!("MaxSizeGuaranteed = {v};\n");
    }
    if let Some(v) = cfg.max_size_nonguaranteed {
        out += &format!("MaxSizeNonguaranteed = {v};\n");
    }
    if let Some(v) = &cfg.min_bandwidth {
        out += &format!("MinBandwidth = \"{}\";\n", escape(v));
    }
    if let Some(v) = &cfg.max_bandwidth {
        out += &format!("MaxBandwidth = \"{}\";\n", escape(v));
    }
    if let Some(v) = cfg.min_error_to_send {
        out += &format!("MinErrorToSend = {v};\n");
    }
    if let Some(v) = cfg.min_error_to_send_near {
        out += &format!("MinErrorToSendNear = {v};\n");
    }
    if !cfg.other_properties.is_empty() {
        out += "\n";
        out += &cfg.other_properties;
        out += "\n";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_main() -> ResolvedMainConfig {
        ResolvedMainConfig {
            hostname: "Test Server".into(),
            max_players: 64,
            force_difficulty: false,
            forced_difficulty: "veteran".into(),
            missions_whitelist: vec![],
            persist_without_players: false,
            use_battleye: false,
            verify_signatures: true,
            skip_lobby: false,
            allow_zeus_composition_scripts: true,
            allow_custom_glasses: false,
            max_ping: Some(300),
            max_packet_loss: None,
            max_desync: None,
            password_admin: "".into(),
            password: "".into(),
            server_command_password: "".into(),
            motd: vec![],
            motd_interval: None,
            other_properties: "".into(),
            admins: vec!["76561198027717871".into()],
            file_patching_exceptions: vec![],
            kick_timeout: vec![(0, 1), (1, 1), (2, 1), (3, 1)],
            allowed_file_patching: 1,
            disable_von: true,
            headless_clients: false,
        }
    }

    #[test]
    fn renders_kick_timeout_pairs() {
        let out = render_main_cfg(&base_main());
        assert!(out.contains("kickTimeout[] = {{ 0, 1 },{ 1, 1 },{ 2, 1 },{ 3, 1 }};"));
    }

    #[test]
    fn empty_kick_timeout_renders_as_empty_braces() {
        let mut cfg = base_main();
        cfg.kick_timeout = vec![];
        let out = render_main_cfg(&cfg);
        assert!(out.contains("kickTimeout[] = {};"));
    }

    #[test]
    fn renders_allowed_file_patching_and_disable_von() {
        let out = render_main_cfg(&base_main());
        assert!(out.contains("allowedFilePatching = 1;"));
        assert!(out.contains("disableVON = 1;"));
    }

    #[test]
    fn renders_verify_signatures_as_2_when_true() {
        let out = render_main_cfg(&base_main());
        assert!(out.contains("verifySignatures = 2;"));
    }

    #[test]
    fn omits_forced_difficulty_when_not_forced() {
        let out = render_main_cfg(&base_main());
        assert!(!out.contains("forcedDifficulty"));
    }

    #[test]
    fn emits_forced_difficulty_when_forced() {
        let mut cfg = base_main();
        cfg.force_difficulty = true;
        cfg.forced_difficulty = "custom".into();
        let out = render_main_cfg(&cfg);
        assert!(out.contains("forcedDifficulty = \"custom\";"));
    }

    #[test]
    fn empty_lists_render_as_empty_braces() {
        let out = render_main_cfg(&base_main());
        assert!(out.contains("filePatchingExceptions[] = {};"));
        assert!(out.contains("motd[] = {};"));
    }

    #[test]
    fn admins_list_renders_quoted() {
        let out = render_main_cfg(&base_main());
        assert!(out.contains("admins[] = {\"76561198027717871\"};"));
    }

    #[test]
    fn basic_cfg_omits_unset_fields_entirely() {
        let cfg = ResolvedBasicConfig {
            max_msg_send: Some(256),
            max_size_guaranteed: None,
            max_size_nonguaranteed: None,
            min_bandwidth: None,
            max_bandwidth: None,
            min_error_to_send: None,
            min_error_to_send_near: None,
            other_properties: "".into(),
        };
        let out = render_basic_cfg(&cfg);
        assert_eq!(out, "MaxMsgSend = 256;\n");
    }

    #[test]
    fn headless_clients_add_localhost_directives() {
        let mut cfg = base_main();
        cfg.headless_clients = true;
        let out = render_main_cfg(&cfg);
        assert!(out.contains("headlessClients[] = {\"127.0.0.1\"};"));
        assert!(out.contains("localClient[] = {\"127.0.0.1\"};"));
    }

    #[test]
    fn no_headless_clients_omits_directives_entirely() {
        let out = render_main_cfg(&base_main());
        assert!(!out.contains("headlessClients"));
        assert!(!out.contains("localClient"));
    }

    #[test]
    fn hostname_placeholder_substitution() {
        assert_eq!(
            substitute_simple(
                "{{prefix}}{{server_name}}{{suffix}}",
                "skua-main",
                "[TEST] ",
                " EU"
            ),
            "[TEST] skua-main EU"
        );
    }
}
