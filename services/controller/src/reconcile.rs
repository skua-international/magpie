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

use crd::{ArmaServer, ArmaServerPhase, ArmaServerStatus, DesiredState, ModSource, ModSourceInput};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, CSIVolumeSource, Container, EnvVar, EnvVarSource, HostPathVolumeSource,
    LocalObjectReference, PodSecurityContext, PodSpec, PodTemplateSpec, SecretKeySelector,
    SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::api::{Api, DeleteParams, Patch, PatchParams};
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
    let api: Api<ArmaServer> = Api::namespaced(client, &ctx.cfg.namespace);

    tokio::spawn(async move {
        Controller::new(api, watcher::Config::default())
            .run(reconcile, error_policy, ctx)
            .for_each(|res| async move {
                if let Err(e) = res {
                    warn!("reconcile error: {e:#}");
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
    ensure_deployment(ctx, obj, &mod_paths, extra_env).await
}

async fn cleanup(obj: &ArmaServer, ctx: &Ctx) -> anyhow::Result<Action> {
    let name = obj.name_any();
    scale_down(ctx, &name).await?;
    Ok(Action::await_change())
}

async fn scale_down(ctx: &Ctx, name: &str) -> anyhow::Result<()> {
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
        // arma_config.rs always writes exactly these two filenames --
        // there's no operator-selectable filename anymore (see
        // CreateServerRequest's own reserved fields 4/5 for why).
        EnvVar {
            name: "ARMA_CONFIG".into(),
            value: Some("main.cfg".into()),
            ..Default::default()
        },
        EnvVar {
            name: "NETWORK_CONFIG".into(),
            value: Some("basic.cfg".into()),
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
    if !obj.spec.params.is_empty() {
        env.push(EnvVar {
            name: "ARMA_PARAMS".into(),
            value: Some(obj.spec.params.join(" ")),
            ..Default::default()
        });
    }
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
