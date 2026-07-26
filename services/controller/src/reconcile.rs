//! `ArmaServer` reconciler. Servers are deployment-like: `spec.desired_state`
//! (`Running`/`Stopped`) is the "should this be up" knob, and `status.phase`
//! tracks how far the reconciler has gotten toward that --
//! `Stopped -> Pending -> Running`, with `Failed` on error. Each
//! `ArmaServer` backs a Kubernetes `Deployment` (not a bare `Pod`) with
//! `strategy: Recreate` -- since it's `hostNetwork: true`, two instances can
//! never coexist on the same port anyway, and using a Deployment means a
//! content/mod change picked up by `UpdateServer`/`StartServer` rolls the
//! server onto a new Pod natively (new PodTemplateSpec -> new ReplicaSet)
//! instead of the reconciler having to delete/recreate a bare Pod by hand.
//!
//! `Pending` used to mean "sync-daemon Claim job in flight, poll until
//! Done" (a `Claiming` phase in between). That's gone: every launcher
//! Pod's own content now comes from a CSI inline ephemeral volume (see
//! `ensure_deployment`'s own doc) that provisions itself -- a fresh
//! read-only btrfs snapshot of sync-daemon's golden tree, taken by
//! services/magpie-csi's `NodePublishVolume` the moment the Pod is
//! actually scheduled -- so there's nothing left for this reconciler to
//! create or poll beforehand at all. `Pending` now just means "resolve
//! mods, render config, apply the Deployment", synchronously, in one
//! reconcile pass.
//!
//! Uses a finalizer so the Deployment is cleaned up before an `ArmaServer`
//! is actually deleted -- deliberately does *not* call sync-daemon's
//! `DeregisterSource` or delete any local mod's files, since a server going
//! away must not silently stop syncing/storing content someone may want
//! kept available (see `service/mod_source.rs`'s `DeleteModSource`, the
//! only place that happens).

use std::sync::Arc;
use std::time::Duration;

use crd::{
    ArmaServer, ArmaServerPhase, ArmaServerStatus, DesiredState, HeadlessClient,
    HeadlessClientPhase, HeadlessClientSpec, HeadlessClientStatus, ModSource, ModSourceInput,
    ModSourcePhase,
};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    CSIVolumeSource, Capabilities, Container, EnvVar, EnvVarSource, ExecAction,
    HostPathVolumeSource, LocalObjectReference, PodSecurityContext, PodSpec, PodTemplateSpec,
    Probe, SecretKeySelector, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::runtime::Controller;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event as FinalizerEvent, finalizer};
use kube::runtime::watcher;
use kube::{Client, Resource, ResourceExt};
use sync_client::SyncClient;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::config::Config;

const FINALIZER_NAME: &str = "arma.skua.io/cleanup";
const FAST_REQUEUE: Duration = Duration::from_secs(5);
const SLOW_REQUEUE: Duration = Duration::from_secs(300);
// Separate from SLOW_REQUEUE deliberately: that one's the steady-state poll
// interval for objects with nothing to do (Running, permanently Failed).
// This one backs off a genuine reconcile error (e.g. sync-daemon briefly
// unreachable mid-restart) -- reusing SLOW_REQUEUE here meant any transient
// blip while still Pending parked the object for 5 minutes before the
// controller looked at it again.
const ERROR_REQUEUE: Duration = Duration::from_secs(10);

/// `kube-runtime`'s `finalizer`/`Controller::run` both require their error
/// type to implement `std::error::Error`, which `anyhow::Error` deliberately
/// doesn't (to avoid blanket-impl conflicts) -- this is a thin wrapper so
/// the rest of the reconciler can keep using `anyhow::Result`/`?` as usual.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
struct ReconcileError(#[from] anyhow::Error);

struct Ctx {
    client: Client,
    cfg: Arc<Config>,
    sync_client: SyncClient,
    pool: sqlx::PgPool,
}

pub fn spawn(client: Client, pool: sqlx::PgPool, cfg: Arc<Config>) -> anyhow::Result<()> {
    let sync_client = SyncClient::new(&cfg.sync_daemon_url)?;
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        cfg,
        sync_client,
        pool,
    });
    let api: Api<ArmaServer> = Api::namespaced(client.clone(), &ctx.cfg.namespace);

    tokio::spawn({
        let ctx = ctx.clone();
        async move {
            Controller::new(api, watcher::Config::default())
                .run(reconcile, error_policy, ctx)
                .for_each(|res| async move {
                    if let Err(e) = res {
                        warn!("reconcile error: {e:#}");
                    }
                })
                .await;
        }
    });

    let hc_api: Api<HeadlessClient> = Api::namespaced(client, &ctx.cfg.namespace);
    tokio::spawn(async move {
        Controller::new(hc_api, watcher::Config::default())
            .run(reconcile_hc, hc_error_policy, ctx)
            .for_each(|res| async move {
                if let Err(e) = res {
                    warn!("headless client reconcile error: {e:#}");
                }
            })
            .await;
    });
    Ok(())
}

type FinalizerError = kube::runtime::finalizer::Error<ReconcileError>;

async fn reconcile(obj: Arc<ArmaServer>, ctx: Arc<Ctx>) -> Result<Action, FinalizerError> {
    let api: Api<ArmaServer> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    finalizer(&api, FINALIZER_NAME, obj, |event| async {
        match event {
            FinalizerEvent::Apply(obj) => apply(&obj, &ctx).await.map_err(ReconcileError),
            FinalizerEvent::Cleanup(obj) => cleanup(&obj, &ctx).await.map_err(ReconcileError),
        }
    })
    .await
}

fn error_policy(_obj: Arc<ArmaServer>, err: &FinalizerError, _ctx: Arc<Ctx>) -> Action {
    error!("reconcile failed: {err}");
    Action::requeue(ERROR_REQUEUE)
}

async fn apply(obj: &ArmaServer, ctx: &Ctx) -> anyhow::Result<Action> {
    let name = obj.name_any();
    let status = obj.status.clone().unwrap_or_default();
    let desired_running = obj.spec.desired_state == DesiredState::Running;

    match status.phase {
        ArmaServerPhase::Stopped if desired_running => {
            // Fire-and-forget, best-effort: makes sure a start actually
            // kicks sync-daemon into motion instead of just hoping
            // something else already has (its own startup sync, a
            // ModSource's first resolve, or an admin registering a mod
            // source through registry) -- otherwise the Pending gate below
            // could wait on a golden tree nothing is actively syncing.
            // Idempotent on sync-daemon's side regardless of whether a
            // sync is already in flight (see SyncContent's own proto doc),
            // so this is safe to call on every start with no dedup needed
            // here.
            if let Err(e) = ctx.sync_client.sync_content().await {
                warn!("{name}: failed to trigger sync-daemon content sync on start: {e:#}");
            }
            set_status(
                ctx,
                &name,
                ArmaServerStatus {
                    phase: ArmaServerPhase::Pending,
                    ..Default::default()
                },
            )
            .await?;
            Ok(Action::requeue(FAST_REQUEUE))
        }
        ArmaServerPhase::Stopped => Ok(Action::await_change()),

        _ if !desired_running => {
            scale_down(ctx, &name).await?;
            // Headless clients have nothing to connect to once the main
            // server is stopped -- deleted outright, not scaled to 0 like
            // the main Deployment: a HeadlessClient CR's own existence
            // *is* its desired state (see ensure_headless_clients' own
            // doc), so there's no separate "scaled down but still
            // declared" state for it to sit in the way the main server's
            // Deployment can.
            delete_all_headless_clients(ctx, &name).await?;
            set_status(
                ctx,
                &name,
                ArmaServerStatus {
                    phase: ArmaServerPhase::Stopped,
                    ..Default::default()
                },
            )
            .await?;
            Ok(Action::await_change())
        }

        ArmaServerPhase::Pending => {
            // A launcher Pod's own content is a CSI snapshot of sync-daemon's
            // golden tree taken the instant it's scheduled -- if that tree
            // is still mid-download (or has never finished a first sync at
            // all), the snapshot is of a partial/incomplete tree and
            // arma3server_x64 fails outright (confirmed live: "Permission
            // denied" spawning it, since steamcmd doesn't finalize a
            // depot's files -- including the binary's own mode -- until its
            // download actually completes). Block here instead of creating
            // the Deployment against that: stay Pending and requeue fast
            // until sync-daemon reports the golden tree is quiescent and
            // has a complete base game.
            match ctx.sync_client.sync_status().await {
                Ok(status) if status.syncing || !status.game_files_ready => {
                    info!(
                        "{name}: waiting for sync-daemon (syncing={}, game_files_ready={})",
                        status.syncing, status.game_files_ready
                    );
                    return Ok(Action::requeue(FAST_REQUEUE));
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("{name}: failed to check sync-daemon status, retrying: {e:#}");
                    return Ok(Action::requeue(ERROR_REQUEUE));
                }
            }

            // The global check above only covers "is anything syncing
            // right now" and the base game -- it says nothing about
            // *this* server's own mod_source_ids. Confirmed live: an
            // ArmaServer created while sync-daemon happened to be
            // globally idle (its own mod not yet queued, or resolved but
            // not yet downloaded) passed the check above, took its CSI
            // btrfs snapshot of the golden tree at that moment, and got
            // permanently stuck with a zero-filled placeholder for a
            // file sync-daemon hadn't written yet -- a snapshot is an
            // independent CoW copy, so it never picks up the golden
            // tree's later, completed write. Block on every one of this
            // server's own mod sources reporting Synced, not just the
            // cluster-wide flags.
            match not_yet_synced_mod_source(ctx, obj).await {
                Ok(Some(pending_name)) => {
                    info!("{name}: waiting for mod source {pending_name} to finish syncing");
                    return Ok(Action::requeue(FAST_REQUEUE));
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("{name}: failed to check mod source sync status, retrying: {e:#}");
                    return Ok(Action::requeue(ERROR_REQUEUE));
                }
            }

            let new_phase = match run_pending(ctx, obj).await {
                Ok(()) => ArmaServerStatus {
                    phase: ArmaServerPhase::Running,
                    message: String::new(),
                },
                Err(e) => ArmaServerStatus {
                    phase: ArmaServerPhase::Failed,
                    message: format!("{e:#}"),
                },
            };
            set_status(ctx, &name, new_phase).await?;
            Ok(Action::requeue(SLOW_REQUEUE))
        }
        ArmaServerPhase::Running => {
            // Steady state -- nothing to do until UpdateServer/StartServer
            // (via the ServerService RPCs) resets phase back to Pending, or
            // the periodic requeue below fires as a plain reconciliation
            // heartbeat. Deliberately doesn't re-resolve mod sources here:
            // that's an explicit, on-demand action now (see the RPCs
            // above), not something steady-state reconciliation does on
            // its own -- avoids surprise rollouts from unrelated reconciles.
            Ok(Action::requeue(SLOW_REQUEUE))
        }
        ArmaServerPhase::Failed => {
            set_status(
                ctx,
                &name,
                ArmaServerStatus {
                    phase: ArmaServerPhase::Pending,
                    ..Default::default()
                },
            )
            .await?;
            Ok(Action::requeue(FAST_REQUEUE))
        }
    }
}

/// The actual `Pending` work: resolve this server's `-mod=` paths, render
/// its config, and apply the Deployment. Split out from `apply`'s own
/// `Pending` arm purely so that arm can catch any failure in one place
/// and report it as `Failed` with a message, instead of the whole
/// three-step sequence needing its own nested error handling.
async fn run_pending(ctx: &Ctx, obj: &ArmaServer) -> anyhow::Result<()> {
    let mod_paths = resolve_mod_paths(ctx, obj).await?;
    let extra_env = crate::arma_config::render_and_write(
        &ctx.client,
        &ctx.pool,
        &ctx.cfg.namespace,
        &ctx.cfg.user_secrets_namespace,
        &ctx.cfg.server_root_base,
        &ctx.cfg.arma_config_baseline,
        obj,
    )
    .await?;
    ensure_deployment(ctx, obj, &mod_paths, extra_env).await?;
    ensure_headless_clients(ctx, obj).await
}

async fn cleanup(obj: &ArmaServer, ctx: &Ctx) -> anyhow::Result<Action> {
    let name = obj.name_any();
    delete_deployment(ctx, &name).await?;
    delete_all_headless_clients(ctx, &name).await?;
    Ok(Action::await_change())
}

/// `desired_state: Stopped` -- scale to 0 rather than deleting outright.
/// `ensure_deployment` always re-applies the full `DeploymentSpec`
/// (`replicas: Some(1)` baked in) via SSA on every `Pending` transition,
/// so a subsequent start already resets this back to 1 with no extra
/// handling needed here -- scaling down is strictly cheaper than a
/// delete + full recreate for the exact same end state, and leaves the
/// object (and its events/history) around for `kubectl describe` while
/// stopped, more in line with how a plain Kubernetes Deployment is
/// normally scaled down rather than destroyed.
async fn scale_down(ctx: &Ctx, name: &str) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    let patch = serde_json::json!({ "spec": { "replicas": 0 } });
    match deployments
        .patch(name, &PatchParams::default(), &Patch::Merge(patch))
        .await
    {
        Ok(_) => info!("{name}: scaled launcher deployment to 0 replicas"),
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Real deletion (the `ArmaServer` itself is going away, via the
/// finalizer) -- unlike a plain stop, there's no reason to leave a
/// 0-replica Deployment object behind for something that no longer
/// exists at all.
async fn delete_deployment(ctx: &Ctx, name: &str) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    match deployments.delete(name, &DeleteParams::default()).await {
        Ok(_) => info!("{name}: deleted launcher deployment"),
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

async fn set_status(ctx: &Ctx, name: &str, status: ArmaServerStatus) -> anyhow::Result<()> {
    let api: Api<ArmaServer> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(patch))
        .await?;
    Ok(())
}

/// This `ArmaServer`'s currently-existing `HeadlessClient` objects --
/// listed and filtered client-side (`spec.server == name`) rather than
/// via a label selector, since the count per server is always small and
/// this avoids having to keep a label in sync with `spec.server`
/// separately.
async fn list_headless_clients(ctx: &Ctx, name: &str) -> anyhow::Result<Vec<HeadlessClient>> {
    let api: Api<HeadlessClient> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    let all = api.list(&ListParams::default()).await?;
    Ok(all
        .items
        .into_iter()
        .filter(|hc| hc.spec.server == name)
        .collect())
}

/// Creates/deletes owned `HeadlessClient` objects (named
/// `<server>-hc-<index>`) to match `spec.headless_clients` -- a
/// HeadlessClient CR's own existence *is* its desired state (unlike the
/// main server, there's no separate `desired_state` knob on it at all),
/// so this is the one place that decides whether each index should
/// exist; the separate HeadlessClient reconciler (`reconcile_hc`) just
/// reacts to whatever already exists, no polling of the owning server's
/// own state needed on that side.
async fn ensure_headless_clients(ctx: &Ctx, obj: &ArmaServer) -> anyhow::Result<()> {
    let name = obj.name_any();
    let api: Api<HeadlessClient> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    let existing = list_headless_clients(ctx, &name).await?;
    let desired = obj.spec.headless_clients;

    for hc in &existing {
        if hc.spec.index >= desired {
            let hc_name = hc.name_any();
            match api.delete(&hc_name, &DeleteParams::default()).await {
                Ok(_) => info!("{name}: deleted excess headless client {hc_name}"),
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    let existing_indices: std::collections::HashSet<u32> =
        existing.iter().map(|hc| hc.spec.index).collect();
    for index in 0..desired {
        if existing_indices.contains(&index) {
            continue;
        }
        let hc_name = format!("{name}-hc-{index}");
        let hc = HeadlessClient {
            metadata: ObjectMeta {
                name: Some(hc_name.clone()),
                owner_references: Some(vec![
                    obj.controller_owner_ref(&())
                        .expect("ArmaServer has a name"),
                ]),
                ..Default::default()
            },
            spec: HeadlessClientSpec {
                server: name.clone(),
                index,
            },
            status: None,
        };
        api.patch(
            &hc_name,
            &PatchParams::apply("arma-controller").force(),
            &Patch::Apply(&hc),
        )
        .await?;
        info!("{name}: created headless client {hc_name}");
    }
    Ok(())
}

async fn delete_all_headless_clients(ctx: &Ctx, name: &str) -> anyhow::Result<()> {
    let api: Api<HeadlessClient> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    for hc in list_headless_clients(ctx, name).await? {
        let hc_name = hc.name_any();
        match api.delete(&hc_name, &DeleteParams::default()).await {
            Ok(_) => info!("{name}: deleted headless client {hc_name}"),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Returns the name of the first of this server's `mod_source_ids` that
/// hasn't reached `ModSourcePhase::Synced` yet, or `None` if they all
/// have (including the trivial case of no mod sources at all). A source
/// that's `Failed` still counts as "not yet synced" here rather than a
/// hard error -- same as the global sync-status check above, this is a
/// wait-and-requeue gate, not a place to fail the whole server over one
/// mod that's struggling; `resolve_mod_paths`/`run_pending` surface a
/// real error later if it never recovers.
async fn not_yet_synced_mod_source(ctx: &Ctx, obj: &ArmaServer) -> anyhow::Result<Option<String>> {
    let mod_sources: Api<ModSource> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    for source_id in &obj.spec.mod_source_ids {
        let source = mod_sources.get(source_id).await.map_err(|e| match e {
            kube::Error::Api(e) if e.code == 404 => {
                anyhow::anyhow!("mod source {source_id} no longer exists")
            }
            e => e.into(),
        })?;
        let phase = source.status.map(|s| s.phase).unwrap_or_default();
        if phase != ModSourcePhase::Synced {
            return Ok(Some(source_id.clone()));
        }
    }
    Ok(None)
}

/// Unions every mod source this server references into `-mod=`-ready
/// paths: `workshop/<id>` (relative to `CLAIM_PATH`) for Steam-backed
/// sources, or an absolute path into the shared local-content volume for
/// local (zip-uploaded) sources.
async fn resolve_mod_paths(ctx: &Ctx, obj: &ArmaServer) -> anyhow::Result<Vec<String>> {
    let mod_sources: Api<ModSource> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    let mut paths = Vec::new();
    for source_id in &obj.spec.mod_source_ids {
        let source = mod_sources.get(source_id).await.map_err(|e| match e {
            kube::Error::Api(e) if e.code == 404 => {
                anyhow::anyhow!("mod source {source_id} no longer exists")
            }
            e => e.into(),
        })?;
        match &source.spec.source {
            ModSourceInput::Local { unique_id } => {
                paths.push(format!("{}/mods/{}", ctx.cfg.local_content_root, unique_id))
            }
            ModSourceInput::SteamUrl(_)
            | ModSourceInput::HtmlUrl(_)
            | ModSourceInput::HtmlContent(_) => {
                let mod_ids = ctx.sync_client.get_source_mods(source_id).await?;
                paths.extend(mod_ids.into_iter().map(|id| format!("workshop/{id}")));
            }
        }
    }
    Ok(paths)
}

/// Fixed in-container mount point for the CSI inline ephemeral content
/// volume every launcher Pod gets (see this function's own doc) -- no
/// longer a per-launch dynamic path (there's no job/claim ID to embed
/// in it anymore), so this can just be a constant.
const CLAIM_PATH: &str = "/arma3/content";

async fn ensure_deployment(
    ctx: &Ctx,
    obj: &ArmaServer,
    mod_paths: &[String],
    extra_env: Vec<(String, String)>,
) -> anyhow::Result<()> {
    let name = obj.name_any();
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);

    let mut env = vec![
        EnvVar {
            name: "CLAIM_PATH".into(),
            value: Some(CLAIM_PATH.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "MODS".into(),
            value: Some(mod_paths.join(";")),
            ..Default::default()
        },
        EnvVar {
            name: "ARMA_CDLC".into(),
            value: Some(obj.spec.cdlc.join(";")),
            ..Default::default()
        },
        EnvVar {
            name: "PORT".into(),
            value: Some(obj.spec.port.to_string()),
            ..Default::default()
        },
        // Consumed by whatever Postgres-backed extension the Arma server
        // process itself loads (not anything this repo owns) -- role
        // provisioned once on controller startup, see
        // postgres_bootstrap.rs.
        EnvVar {
            name: "DATABASE_HOST".into(),
            value: Some(ctx.cfg.app_postgres_host.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "DATABASE_PORT".into(),
            value: Some(ctx.cfg.app_postgres_port.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "DATABASE_USER".into(),
            value: Some(ctx.cfg.app_postgres_role.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "DATABASE_NAME".into(),
            value: Some(ctx.cfg.app_postgres_database.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "DATABASE_PASSWORD".into(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: ctx.cfg.app_postgres_secret_name.clone(),
                    key: "POSTGRES_PASSWORD".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    // From the merged arma-config ConfigMap's own `env.*` keys -- see
    // arma_config::extract_env_vars. Appended last so an operator can
    // override any of the fixed vars above via the same ConfigMap
    // mechanism if they genuinely need to (e.g. re-pointing MODS).
    for (key, value) in extra_env {
        env.push(EnvVar {
            name: key,
            value: Some(value),
            ..Default::default()
        });
    }

    let labels: std::collections::BTreeMap<String, String> = [
        ("app".to_string(), "arma-server".to_string()),
        ("armaserver".to_string(), name.clone()),
    ]
    .into();

    // Purely a scrape hint for the operator's own exporter on
    // spec.metrics (see ArmaServerSpec.metrics's own doc) -- plain
    // prometheus.io/* annotations rather than a PodMonitor/ServiceMonitor
    // CRD, same discovery mechanism this chart's own services advertise
    // themselves with (see charts/magpie/templates/*-deployment.yaml),
    // so it doesn't depend on prometheus-operator's CRDs existing in the
    // cluster just because an operator wants their own exporter scraped.
    let mut pod_annotations = std::collections::BTreeMap::new();
    if let Some(m) = &obj.spec.metrics {
        pod_annotations.insert("prometheus.io/scrape".to_string(), "true".to_string());
        pod_annotations.insert("prometheus.io/port".to_string(), m.port.to_string());
        pod_annotations.insert(
            "prometheus.io/path".to_string(),
            m.path_or_default().to_string(),
        );
    }
    // A fresh value on every call (every Pending transition -- see
    // run_pending) so the PodTemplateSpec is never byte-identical to
    // its predecessor even when nothing else changed (same mods, same
    // env). Without this, Kubernetes has no reason to actually replace
    // the Pod on a plain resync (StartServer resetting phase back to
    // Pending with an otherwise-unchanged spec), and the CSI inline
    // ephemeral content volume (see this Pod spec's own "content"
    // volume below) only ever provisions a fresh snapshot when a new
    // Pod actually gets scheduled -- an unchanged Pod means stale
    // content, defeating the entire point of a resync.
    pod_annotations.insert(
        "magpie.skua.io/content-generation".to_string(),
        Uuid::now_v7().to_string(),
    );

    let deployment = Deployment {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            labels: Some(labels.clone()),
            owner_references: Some(vec![
                obj.controller_owner_ref(&())
                    .expect("ArmaServer has a name"),
            ]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            },
            // hostNetwork Pods can't run two-at-a-time on the same port on
            // the same node anyway (this project's single-node k3s scope
            // makes that doubly true) -- Recreate guarantees the old Pod is
            // fully torn down before the new one (new content snapshot/
            // mod list) starts, rather than RollingUpdate's default
            // overlap.
            strategy: Some(k8s_openapi::api::apps::v1::DeploymentStrategy {
                type_: Some("Recreate".into()),
                rolling_update: None,
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    annotations: Some(pod_annotations),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    host_network: Some(true),
                    // Non-root here too, like every other Pod this chart
                    // creates -- what looked like a genuine "needs UID 0"
                    // requirement (three isolated tests: plain non-root
                    // crashes, non-root + CAP_SYS_NICE crashes
                    // identically, non-root + every capability root has
                    // still crashes identically) turned out to be a
                    // single missing /etc/passwd entry for the fixed
                    // nonroot UID: confirmed live that SteamAPI_Init()
                    // crashes outright (not gracefully) when getpwuid()
                    // has nothing to return for the running UID, and
                    // that a real useradd-created entry for that UID
                    // fixes it with zero other changes. The launcher
                    // image moved to distroless:nonroot specifically
                    // because it bakes in exactly that passwd entry for
                    // its own nonroot UID for free -- confirmed live
                    // (this securityContext, ALL capabilities dropped)
                    // against the real content volume: arma3server_x64
                    // starts, loads mods, and binds its game port
                    // exactly as it did running as root.
                    security_context: Some(PodSecurityContext {
                        run_as_non_root: Some(true),
                        ..Default::default()
                    }),
                    image_pull_secrets: (!ctx.cfg.image_pull_secrets.is_empty()).then(|| {
                        ctx.cfg
                            .image_pull_secrets
                            .iter()
                            .map(|name| LocalObjectReference { name: name.clone() })
                            .collect()
                    }),
                    containers: vec![Container {
                        name: "launcher".into(),
                        image: Some(ctx.cfg.launcher_image.clone()),
                        security_context: Some(SecurityContext {
                            allow_privilege_escalation: Some(false),
                            capabilities: Some(Capabilities {
                                drop: Some(vec!["ALL".into()]),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        // Explicit, not left to Kubernetes' default: a
                        // `:latest`-tagged image (this project's own
                        // convention -- see LAUNCHER_IMAGE) defaults to
                        // `Always` otherwise, which forces a real registry
                        // pull on every single server create/restart even
                        // when the exact same image digest is already
                        // present on the node (e.g. imported directly via
                        // `ctr images import` during local testing, or
                        // just already cached from the last pull).
                        image_pull_policy: Some("IfNotPresent".into()),
                        env: Some(env),
                        volume_mounts: Some(vec![
                            VolumeMount {
                                name: "content".into(),
                                mount_path: CLAIM_PATH.into(),
                                // Read-write, not read-only: nothing
                                // arma3server might write in here (temp
                                // files, locks, whatever) needs to
                                // persist past this Pod's own lifetime
                                // anyway -- it's a fresh CoW snapshot
                                // either way -- so there's no reason to
                                // risk it hitting an unexpected EROFS.
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "server-root".into(),
                                mount_path: "/arma3/server".into(),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "local-content".into(),
                                mount_path: ctx.cfg.local_content_root.clone(),
                                read_only: Some(true),
                                ..Default::default()
                            },
                            // Missions aren't server-scoped -- every upload
                            // is available to every server, mounted
                            // directly into Arma's well-known mission
                            // directory.
                            VolumeMount {
                                name: "local-content".into(),
                                mount_path: "/arma3/server/mpmissions".into(),
                                sub_path: Some("missions".into()),
                                read_only: Some(true),
                                ..Default::default()
                            },
                        ]),
                        // A real Source-engine A2S_INFO round trip against
                        // arma3server_x64's own Steam query port (see
                        // healthcheck.rs's own doc), not just "is PID 1
                        // still running" -- a hung/deadlocked-but-alive
                        // process would pass a bare process check forever.
                        // No httpGet/tcpSocket option exists for this:
                        // Kubernetes probes have no native UDP support, so
                        // this has to be exec'd. Long initialDelay/period --
                        // a cold start genuinely takes ~10s+ just for
                        // PhysX/movesType init (confirmed live) before the
                        // query port is even listening, and this is not a
                        // service that benefits from a tight probe cadence.
                        readiness_probe: Some(Probe {
                            exec: Some(ExecAction {
                                command: Some(vec!["/launcher".into(), "healthcheck".into()]),
                            }),
                            initial_delay_seconds: Some(20),
                            period_seconds: Some(10),
                            timeout_seconds: Some(5),
                            failure_threshold: Some(6),
                            ..Default::default()
                        }),
                        liveness_probe: Some(Probe {
                            exec: Some(ExecAction {
                                command: Some(vec!["/launcher".into(), "healthcheck".into()]),
                            }),
                            initial_delay_seconds: Some(60),
                            period_seconds: Some(30),
                            timeout_seconds: Some(5),
                            // Generous on purpose -- a long mission
                            // load/save or save-related hitch stalling
                            // query responses briefly is a false positive
                            // this shouldn't restart the whole server
                            // over.
                            failure_threshold: Some(5),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    volumes: Some(vec![
                        // CSI inline ephemeral volume -- no PVC/PV object
                        // at all (see services/magpie-csi's
                        // NodePublishVolume doc). Kubelet calls
                        // NodePublishVolume on this directly the moment
                        // this Pod is scheduled, which is where the
                        // fresh btrfs snapshot of sync-daemon's golden
                        // content tree actually gets taken -- nothing
                        // before that point (not CreateVolume, not this
                        // reconciler) provisions anything.
                        Volume {
                            name: "content".into(),
                            csi: Some(CSIVolumeSource {
                                driver: "csi.magpie.skua.io".into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "local-content".into(),
                            host_path: Some(HostPathVolumeSource {
                                path: ctx.cfg.local_content_host_path.clone(),
                                type_: Some("DirectoryOrCreate".into()),
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "server-root".into(),
                            host_path: Some(HostPathVolumeSource {
                                path: format!("{}/{name}", ctx.cfg.server_root_base),
                                type_: Some("DirectoryOrCreate".into()),
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    deployments
        .patch(
            &name,
            &PatchParams::apply("arma-controller").force(),
            &Patch::Apply(&deployment),
        )
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------
// HeadlessClient reconciler. A separate Controller loop (see spawn's own
// doc), not a sub-step of the ArmaServer reconciler above -- each
// HeadlessClient CR's own existence *is* its desired state (created/
// deleted by ensure_headless_clients/delete_all_headless_clients above),
// so this side just reacts to whatever already exists: apply's job is
// solely "make sure this CR's own Deployment exists and matches", no
// separate Stopped/Pending/Running state machine needed the way the
// main server has one.
// ---------------------------------------------------------------------

const HC_FINALIZER_NAME: &str = "arma.skua.io/headless-client-cleanup";

type HcFinalizerError = kube::runtime::finalizer::Error<ReconcileError>;

async fn reconcile_hc(obj: Arc<HeadlessClient>, ctx: Arc<Ctx>) -> Result<Action, HcFinalizerError> {
    let api: Api<HeadlessClient> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    finalizer(&api, HC_FINALIZER_NAME, obj, |event| async {
        match event {
            FinalizerEvent::Apply(obj) => hc_apply(&obj, &ctx).await.map_err(ReconcileError),
            FinalizerEvent::Cleanup(obj) => hc_cleanup(&obj, &ctx).await.map_err(ReconcileError),
        }
    })
    .await
}

fn hc_error_policy(_obj: Arc<HeadlessClient>, err: &HcFinalizerError, _ctx: Arc<Ctx>) -> Action {
    error!("headless client reconcile failed: {err}");
    Action::requeue(ERROR_REQUEUE)
}

async fn hc_apply(obj: &HeadlessClient, ctx: &Ctx) -> anyhow::Result<Action> {
    let name = obj.name_any();
    let servers: Api<ArmaServer> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    // Should only ever be missing transiently, if at all: the owning
    // ArmaServer's own finalizer deletes every one of its HeadlessClients
    // (delete_all_headless_clients) before it finishes removing itself,
    // so by the time this object's owner is truly gone, this object
    // itself should already have its own deletionTimestamp set --
    // meaning FinalizerEvent::Cleanup fires instead of Apply, and this
    // lookup is never reached at all in the normal flow.
    let server = servers.get(&obj.spec.server).await.map_err(|e| match e {
        kube::Error::Api(e) if e.code == 404 => {
            anyhow::anyhow!("owning ArmaServer {} no longer exists", obj.spec.server)
        }
        e => e.into(),
    })?;

    let mod_paths = resolve_mod_paths(ctx, &server).await?;
    // A headless client is just another connecting client as far as
    // password[]/launch flags are concerned -- resolved independently
    // here (not passed through from the main server's own last
    // render_and_write call) so this reconciler stays self-contained and
    // correct even if it runs out of step with the ArmaServer's own
    // reconcile pass (e.g. right after a controller restart, before
    // either side's watch cache has settled).
    let launch_config = crate::arma_config::resolve_headless_client_config(
        &ctx.client,
        &ctx.cfg.namespace,
        &ctx.cfg.user_secrets_namespace,
        &ctx.cfg.arma_config_baseline,
        &server,
    )
    .await?;

    let new_status = match ensure_hc_deployment(ctx, obj, &server, &mod_paths, &launch_config).await
    {
        Ok(()) => HeadlessClientStatus {
            phase: HeadlessClientPhase::Running,
            message: String::new(),
        },
        Err(e) => HeadlessClientStatus {
            phase: HeadlessClientPhase::Failed,
            message: format!("{e:#}"),
        },
    };
    set_hc_status(ctx, &name, new_status).await?;
    Ok(Action::requeue(SLOW_REQUEUE))
}

async fn hc_cleanup(obj: &HeadlessClient, ctx: &Ctx) -> anyhow::Result<Action> {
    let name = obj.name_any();
    delete_hc_deployment(ctx, &name).await?;
    Ok(Action::await_change())
}

async fn set_hc_status(ctx: &Ctx, name: &str, status: HeadlessClientStatus) -> anyhow::Result<()> {
    let api: Api<HeadlessClient> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(patch))
        .await?;
    Ok(())
}

async fn delete_hc_deployment(ctx: &Ctx, name: &str) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);
    match deployments.delete(name, &DeleteParams::default()).await {
        Ok(_) => info!("{name}: deleted headless client deployment"),
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Builds a headless client's own Deployment -- same content (mods,
/// CDLC) and `hostNetwork: true` as the owning server (see
/// `crd::HeadlessClientSpec`'s own "tumor pod" doc), own profile
/// (`<server>-hc-<index>`, this object's own name, already unique by
/// construction), launched with `-client` instead of hosting anything.
/// Shares the owning server's own `server-root` hostPath (so its
/// `-profiles=` lands in the same directory) rather than getting a
/// separate one; does *not* share its `content` volume literally (CSI
/// inline ephemeral volumes are inherently Pod-scoped) -- gets its own
/// fresh snapshot of the same golden tree instead, which is
/// indistinguishable in practice since it's read-only content either way.
async fn ensure_hc_deployment(
    ctx: &Ctx,
    obj: &HeadlessClient,
    server: &ArmaServer,
    mod_paths: &[String],
    launch_config: &crate::arma_config::HeadlessClientLaunchConfig,
) -> anyhow::Result<()> {
    let name = obj.name_any();
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);

    let mut env = vec![
        EnvVar {
            name: "CLAIM_PATH".into(),
            value: Some(CLAIM_PATH.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "MODS".into(),
            value: Some(mod_paths.join(";")),
            ..Default::default()
        },
        EnvVar {
            name: "ARMA_CDLC".into(),
            value: Some(server.spec.cdlc.join(";")),
            ..Default::default()
        },
        // The owning server's own port -- an HC never binds this itself
        // (see this Deployment's own doc: no ports/probes at all), it's
        // only ever used as the -port= value alongside -connect= so Arma
        // associates this client with that specific local server.
        EnvVar {
            name: "PORT".into(),
            value: Some(server.spec.port.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "ARMA_PROFILE".into(),
            value: Some(name.clone()),
            ..Default::default()
        },
        // Presence of this var is what tells launch.rs to run in
        // headless-client mode at all (see launch.rs's own doc) --
        // hardcoded to loopback, not discovered per-server, because
        // every HC Pod is hostNetwork: true on this single-node cluster,
        // same as the owning server -- see HeadlessClientSpec's own doc
        // for why that makes 127.0.0.1 always correct rather than
        // something that needs resolving live.
        EnvVar {
            name: "ARMA_CLIENT_CONNECT".into(),
            value: Some("127.0.0.1".into()),
            ..Default::default()
        },
        EnvVar {
            name: "ARMA_LIMITFPS".into(),
            value: Some(launch_config.limit_fps.to_string()),
            ..Default::default()
        },
    ];
    if !launch_config.additional_params.is_empty() {
        env.push(EnvVar {
            name: "ARMA_PARAMS".into(),
            value: Some(launch_config.additional_params.clone()),
            ..Default::default()
        });
    }
    if !launch_config.password.is_empty() {
        env.push(EnvVar {
            name: "ARMA_SERVER_PASSWORD".into(),
            value: Some(launch_config.password.clone()),
            ..Default::default()
        });
    }

    let labels: std::collections::BTreeMap<String, String> = [
        ("app".to_string(), "arma-headless-client".to_string()),
        ("armaserver".to_string(), server.name_any()),
        ("headlessclient".to_string(), name.clone()),
    ]
    .into();

    let mut pod_annotations = std::collections::BTreeMap::new();
    // Same reasoning as ensure_deployment's own identical annotation --
    // forces a fresh Pod (and so a fresh CSI content snapshot) on every
    // resync instead of Kubernetes deciding nothing changed.
    pod_annotations.insert(
        "magpie.skua.io/content-generation".to_string(),
        Uuid::now_v7().to_string(),
    );

    let deployment = Deployment {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            labels: Some(labels.clone()),
            owner_references: Some(vec![
                obj.controller_owner_ref(&())
                    .expect("HeadlessClient has a name"),
            ]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            },
            strategy: Some(k8s_openapi::api::apps::v1::DeploymentStrategy {
                type_: Some("Recreate".into()),
                rolling_update: None,
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    annotations: Some(pod_annotations),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    host_network: Some(true),
                    security_context: Some(PodSecurityContext {
                        run_as_non_root: Some(true),
                        ..Default::default()
                    }),
                    image_pull_secrets: (!ctx.cfg.image_pull_secrets.is_empty()).then(|| {
                        ctx.cfg
                            .image_pull_secrets
                            .iter()
                            .map(|name| LocalObjectReference { name: name.clone() })
                            .collect()
                    }),
                    containers: vec![Container {
                        name: "launcher".into(),
                        image: Some(ctx.cfg.launcher_image.clone()),
                        security_context: Some(SecurityContext {
                            allow_privilege_escalation: Some(false),
                            capabilities: Some(Capabilities {
                                drop: Some(vec!["ALL".into()]),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        image_pull_policy: Some("IfNotPresent".into()),
                        env: Some(env),
                        volume_mounts: Some(vec![
                            VolumeMount {
                                name: "content".into(),
                                mount_path: CLAIM_PATH.into(),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "server-root".into(),
                                mount_path: "/arma3/server".into(),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "local-content".into(),
                                mount_path: ctx.cfg.local_content_root.clone(),
                                read_only: Some(true),
                                ..Default::default()
                            },
                        ]),
                        // No readiness/liveness probes -- the default
                        // healthcheck is a Steam A2S_INFO query against a
                        // hosted server's own Steam query port (see
                        // healthcheck.rs), which doesn't apply here at
                        // all (a -client instance hosts nothing to
                        // query). Kubernetes' own container-exit-code
                        // restart already recovers a crashed HC without
                        // needing a probe on top of that.
                        ..Default::default()
                    }],
                    volumes: Some(vec![
                        Volume {
                            name: "content".into(),
                            csi: Some(CSIVolumeSource {
                                driver: "csi.magpie.skua.io".into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "local-content".into(),
                            host_path: Some(HostPathVolumeSource {
                                path: ctx.cfg.local_content_host_path.clone(),
                                type_: Some("DirectoryOrCreate".into()),
                            }),
                            ..Default::default()
                        },
                        // The owning server's own server-root path, not a
                        // separate one -- shares its configs/profiles
                        // directory (this HC's own -profiles= target).
                        Volume {
                            name: "server-root".into(),
                            host_path: Some(HostPathVolumeSource {
                                path: format!("{}/{}", ctx.cfg.server_root_base, server.name_any()),
                                type_: Some("DirectoryOrCreate".into()),
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    deployments
        .patch(
            &name,
            &PatchParams::apply("arma-controller").force(),
            &Patch::Apply(&deployment),
        )
        .await?;
    Ok(())
}
