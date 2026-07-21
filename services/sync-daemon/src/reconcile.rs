//! `ModSource` reconciler. Every kind except `Local` gets resolved here
//! (candidate IDs -> a flat mod list, via the exact same
//! `Shared::register_source_impl` the `RegisterSource` RPC uses) --
//! `Local` sources are fully handled by `services/registry` itself at
//! creation time (extracting a zip needs no Steam session, so there's
//! nothing for this reconciler to do), and are skipped here entirely.
//!
//! Resolving only means "figure out what this source actually refers to"
//! -- it never itself downloads anything. A successful resolve returns a
//! slow periodic requeue (not `await_change`) specifically so upstream
//! collection-membership drift keeps getting caught even for a source
//! nothing is actively re-registering right now -- this subsumes what
//! used to be a separate standalone poller task.
//!
//! Uses a finalizer so a deleted `ModSource` cleans up sync-daemon's own
//! `sources`/`source_mods` SQLite rows before the object is actually
//! removed -- mirrors `services/controller`'s own finalizer pattern for
//! its Deployments.

use std::sync::Arc;
use std::time::Duration;

use crd::{ModSource, ModSourceInput, ModSourcePhase, ModSourceStatus};
use futures::StreamExt;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::Controller;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event as FinalizerEvent, finalizer};
use kube::runtime::watcher;
use kube::{Client, ResourceExt};
use tracing::{error, info, warn};

use crate::service::Shared;

const FINALIZER_NAME: &str = "arma.skua.io/sync-daemon-cleanup";
const FAST_REQUEUE: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
struct ReconcileError(#[from] anyhow::Error);

struct Ctx {
    client: Client,
    namespace: String,
    drift_requeue: Duration,
    shared: Arc<Shared>,
}

pub fn spawn(client: Client, namespace: String, drift_requeue: Duration, shared: Arc<Shared>) {
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        namespace: namespace.clone(),
        drift_requeue,
        shared,
    });
    let api: Api<ModSource> = Api::namespaced(client, &namespace);

    tokio::spawn(async move {
        Controller::new(api, watcher::Config::default())
            .run(reconcile, error_policy, ctx)
            .for_each(|res| async move {
                if let Err(e) = res {
                    warn!("mod source reconcile error: {e:#}");
                }
            })
            .await;
    });
}

type FinalizerError = kube::runtime::finalizer::Error<ReconcileError>;

async fn reconcile(obj: Arc<ModSource>, ctx: Arc<Ctx>) -> Result<Action, FinalizerError> {
    let api: Api<ModSource> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    finalizer(&api, FINALIZER_NAME, obj, |event| async {
        match event {
            FinalizerEvent::Apply(obj) => apply(&obj, &ctx).await.map_err(ReconcileError),
            FinalizerEvent::Cleanup(obj) => cleanup(&obj, &ctx).await.map_err(ReconcileError),
        }
    })
    .await
}

fn error_policy(_obj: Arc<ModSource>, err: &FinalizerError, ctx: Arc<Ctx>) -> Action {
    error!("mod source reconcile failed: {err}");
    Action::requeue(ctx.drift_requeue)
}

async fn apply(obj: &ModSource, ctx: &Ctx) -> anyhow::Result<Action> {
    let ModSourceInput::Local { .. } = &obj.spec.source else {
        return resolve(obj, ctx).await;
    };
    // Fully handled by registry at creation time -- nothing to reconcile.
    Ok(Action::await_change())
}

async fn resolve(obj: &ModSource, ctx: &Ctx) -> anyhow::Result<Action> {
    let name = obj.name_any();
    // Checked before this reconcile's own set_status call below -- true
    // only for a source that was already fully resolved going into this
    // reconcile (i.e. a steady-state drift recheck, not this source's
    // first-ever resolve).
    let was_already_synced = obj
        .status
        .as_ref()
        .is_some_and(|s| s.phase == ModSourcePhase::Synced);

    let (candidate_ids, kind_hint) = match &obj.spec.source {
        ModSourceInput::SteamUrl(url) => {
            let id = workshop_parse::extract_single_id(url).ok_or_else(|| {
                anyhow::anyhow!("not a recognizable Workshop filedetails URL: {url}")
            })?;
            (vec![id], None)
        }
        ModSourceInput::HtmlUrl(url) => {
            let ids = workshop_parse::extract_candidate_ids(url)
                .await
                .map_err(|e| anyhow::anyhow!("{e:#}"))?;
            (ids, Some("preset"))
        }
        ModSourceInput::HtmlContent(html) => {
            let ids = workshop_parse::parse_preset_html(html);
            if ids.is_empty() {
                anyhow::bail!("no filedetails links found in html_content");
            }
            (ids, Some("preset"))
        }
        ModSourceInput::Local { .. } => unreachable!("handled by apply() before calling resolve()"),
    };

    match ctx.shared.register_source_impl(&candidate_ids, &name).await {
        Ok(outcome) => {
            let resolved_mod_ids: Vec<u64> = outcome.mods.iter().map(|m| m.mod_id).collect();
            let sizes = ctx.shared.sync_state.list_synced_mods();
            let size_bytes = resolved_mod_ids
                .iter()
                .filter_map(|id| sizes.iter().find(|m| m.mod_id == *id).map(|m| m.size_bytes))
                .sum();

            // A SteamUrl's kind is "collection" exactly when Steam itself
            // reports the candidate's own file_type as a collection --
            // *not* whether the resolved set changed shape relative to
            // candidate_ids (confirmed live: a plain mod with required
            // items, e.g. ACE3 depending on CBA_A3, also grows the
            // resolved set via `children`, without the candidate itself
            // being a collection at all -- see
            // steam::ResolveOutcome::candidate_is_collection's own doc).
            // HtmlUrl/HtmlContent are always presets.
            let kind = kind_hint
                .unwrap_or(if outcome.root_is_collection == Some(true) {
                    "collection"
                } else {
                    "mod"
                })
                .to_string();

            set_status(
                ctx,
                &name,
                ModSourceStatus {
                    phase: ModSourcePhase::Synced,
                    kind,
                    display_name: outcome.root_title,
                    resolved_mod_ids,
                    size_bytes,
                    message: String::new(),
                },
            )
            .await?;
            info!("{name}: resolved");

            // Auto-sync content the moment a source first resolves,
            // rather than waiting for an operator to hit SyncModSource or
            // for some server to start (the only two triggers before
            // this). Gated on was_already_synced so a routine drift
            // recheck of an already-synced source doesn't re-download on
            // every requeue -- steam-sync's own chunk-diffing already
            // makes re-syncs cheap, but there's no reason to pay even
            // that cost on a timer for content that hasn't changed.
            // Spawned, not awaited: a fresh mod's download can take a
            // while and shouldn't hold up this reconcile's own return.
            if !was_already_synced {
                let shared = ctx.shared.clone();
                let name = name.clone();
                tokio::spawn(async move {
                    info!("{name}: syncing content after first resolve");
                    match shared.sync_content().await {
                        Ok(()) => info!("{name}: content synced"),
                        Err(e) => warn!("{name}: failed to auto-sync content: {e:#}"),
                    }
                });
            }

            Ok(Action::requeue(ctx.drift_requeue))
        }
        Err(e) => {
            set_status(
                ctx,
                &name,
                ModSourceStatus {
                    phase: ModSourcePhase::Failed,
                    message: format!("{e:#}"),
                    ..Default::default()
                },
            )
            .await?;
            warn!("{name}: failed to resolve: {e:#}");
            Ok(Action::requeue(FAST_REQUEUE))
        }
    }
}

async fn cleanup(obj: &ModSource, ctx: &Ctx) -> anyhow::Result<Action> {
    let name = obj.name_any();
    if !matches!(obj.spec.source, ModSourceInput::Local { .. }) {
        ctx.shared.sync_state.delete_source(&name)?;
    }
    Ok(Action::await_change())
}

async fn set_status(ctx: &Ctx, name: &str, status: ModSourceStatus) -> anyhow::Result<()> {
    let api: Api<ModSource> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(patch))
        .await?;
    Ok(())
}
