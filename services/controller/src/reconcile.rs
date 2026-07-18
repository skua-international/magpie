//! `ArmaServer` reconciler. Servers are deployment-like: `spec.desired_state`
//! (`Running`/`Stopped`) is the "should this be up" knob, and `status.phase`
//! tracks how far the reconciler has gotten toward that --
//! `Stopped -> Pending -> Claiming -> Running`, with `Failed` on error.
//! Each `ArmaServer` backs a Kubernetes `Deployment` (not a bare `Pod`) with
//! `strategy: Recreate` -- since it's `hostNetwork: true`, two instances can
//! never coexist on the same port anyway, and using a Deployment means a
//! content/mod change picked up by `UpdateServer`/`StartServer` rolls the
//! server onto a new Pod natively (new PodTemplateSpec -> new ReplicaSet)
//! instead of the reconciler having to delete/recreate a bare Pod by hand.
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
    Container, EnvVar, HostPathVolumeSource, LocalObjectReference, PodSpec, PodTemplateSpec,
    Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::api::{Api, DeleteParams, Patch, PatchParams};
use kube::runtime::Controller;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event as FinalizerEvent, finalizer};
use kube::runtime::watcher;
use kube::{Client, Resource, ResourceExt};
use sync_client::{ClaimStatus, SyncClient};
use tracing::{error, info, warn};

use crate::config::Config;

const FINALIZER_NAME: &str = "arma.skua.io/cleanup";
const FAST_REQUEUE: Duration = Duration::from_secs(5);
const SLOW_REQUEUE: Duration = Duration::from_secs(300);

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
}

pub fn spawn(client: Client, cfg: Arc<Config>) -> anyhow::Result<()> {
    let sync_client = SyncClient::new(&cfg.sync_daemon_url)?;
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        cfg,
        sync_client,
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
    Action::requeue(SLOW_REQUEUE)
}

async fn apply(obj: &ArmaServer, ctx: &Ctx) -> anyhow::Result<Action> {
    let name = obj.name_any();
    let status = obj.status.clone().unwrap_or_default();
    let desired_running = obj.spec.desired_state == DesiredState::Running;

    match status.phase {
        ArmaServerPhase::Stopped if desired_running => {
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
            let job_id = ctx.sync_client.claim().await?;
            set_status(
                ctx,
                &name,
                ArmaServerStatus {
                    phase: ArmaServerPhase::Claiming,
                    claim_path: job_id,
                    message: String::new(),
                },
            )
            .await?;
            Ok(Action::requeue(FAST_REQUEUE))
        }
        ArmaServerPhase::Claiming => {
            // `status.claim_path` is repurposed to hold the in-flight job ID
            // while claiming -- overwritten with the real claim path once
            // `Done`, so it never leaks the job ID past this phase. The
            // Deployment (if one already exists, from a prior Running
            // period) is left untouched until we have a real claim path to
            // roll it onto -- a resync-in-progress never tears down a
            // currently-serving instance speculatively.
            let job_id = status.claim_path.clone();
            match ctx.sync_client.claim_status(&job_id).await? {
                ClaimStatus::Running => Ok(Action::requeue(FAST_REQUEUE)),
                ClaimStatus::Failed { error } => {
                    set_status(
                        ctx,
                        &name,
                        ArmaServerStatus {
                            phase: ArmaServerPhase::Failed,
                            claim_path: String::new(),
                            message: error,
                        },
                    )
                    .await?;
                    Ok(Action::requeue(SLOW_REQUEUE))
                }
                ClaimStatus::Done { claim_path } => {
                    let mod_paths = resolve_mod_paths(ctx, obj).await?;
                    ensure_deployment(ctx, obj, &claim_path, &mod_paths).await?;
                    set_status(
                        ctx,
                        &name,
                        ArmaServerStatus {
                            phase: ArmaServerPhase::Running,
                            claim_path,
                            message: String::new(),
                        },
                    )
                    .await?;
                    Ok(Action::requeue(SLOW_REQUEUE))
                }
            }
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

async fn ensure_deployment(
    ctx: &Ctx,
    obj: &ArmaServer,
    claim_path: &str,
    mod_paths: &[String],
) -> anyhow::Result<()> {
    let name = obj.name_any();
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ctx.cfg.namespace);

    let mut env = vec![
        EnvVar {
            name: "CLAIM_PATH".into(),
            value: Some(claim_path.to_string()),
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
        EnvVar {
            name: "ARMA_CONFIG".into(),
            value: Some(obj.spec.arma_config.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "NETWORK_CONFIG".into(),
            value: Some(obj.spec.network_config.clone()),
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

    let labels: std::collections::BTreeMap<String, String> = [
        ("app".to_string(), "arma-server".to_string()),
        ("armaserver".to_string(), name.clone()),
    ]
    .into();

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
            // fully torn down before the new one (new claim path/mod list)
            // starts, rather than RollingUpdate's default overlap.
            strategy: Some(k8s_openapi::api::apps::v1::DeploymentStrategy {
                type_: Some("Recreate".into()),
                rolling_update: None,
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    host_network: Some(true),
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
                                name: "claims".into(),
                                mount_path: ctx.cfg.claims_root.clone(),
                                read_only: Some(true),
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
                        Volume {
                            name: "claims".into(),
                            host_path: Some(HostPathVolumeSource {
                                path: ctx.cfg.claims_host_path.clone(),
                                type_: Some("DirectoryOrCreate".into()),
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
