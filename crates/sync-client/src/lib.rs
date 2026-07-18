//! Thin wrapper around a generated Connect client for sync-daemon's
//! `SyncService`, used only by the reconciler -- the controller never talks
//! to Steam itself, only in terms of candidate/resolved mod IDs.

use connectrpc::client::{ClientConfig, HttpClient};
use protocol::proto::sync::v1::{
    ClaimJobState, ClaimRequest, DeregisterSourceRequest, GetClaimStatusRequest,
    GetSourceModsRequest, GetSyncStatsRequest, GetSyncedModRequest, InvalidateModRequest,
    ListSyncedModsRequest, RefreshSourceRequest, RefreshSteamAuthRequest, RegisterSourceRequest,
    SyncServiceClient,
};

pub struct SyncClient {
    inner: SyncServiceClient<HttpClient>,
}

pub enum ClaimStatus {
    Running,
    Done { claim_path: String },
    Failed { error: String },
}

pub struct SyncedMod {
    pub mod_id: u64,
    pub manifest_id: u64,
    pub size_bytes: u64,
    pub title: String,
}

pub struct SyncStats {
    pub mods_bytes: u64,
    pub game_files_bytes: u64,
}

pub enum RefreshSteamAuthResult {
    NeedsGuard { guard_type: String },
    Success,
}

pub struct RegisterSourceResult {
    pub mod_ids: Vec<u64>,
    /// The registered candidate's own Workshop title, when `candidate_ids`
    /// had exactly one entry (a single mod or collection link) -- empty
    /// otherwise (e.g. a multi-mod preset source).
    pub root_title: String,
}

impl SyncClient {
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        let uri: http::Uri = base_url.parse()?;
        let transport = HttpClient::plaintext();
        let config = ClientConfig::new(uri);
        Ok(Self {
            inner: SyncServiceClient::new(transport, config),
        })
    }

    /// Resolve `candidate_ids` and register/refresh them as `source_id`.
    pub async fn register_source(
        &self,
        source_id: &str,
        candidate_ids: &[u64],
    ) -> anyhow::Result<RegisterSourceResult> {
        let response = self
            .inner
            .register_source(RegisterSourceRequest {
                source_id: source_id.to_string(),
                candidate_ids: candidate_ids.to_vec(),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("RegisterSource failed: {e}"))?;
        let view = response.view();
        Ok(RegisterSourceResult {
            mod_ids: view.mods.iter().map(|m| m.mod_id).collect(),
            root_title: view.root_title.to_string(),
        })
    }

    /// Read `source_id`'s currently resolved mod list, no Steam calls
    /// (used by the reconciler to build a server's own `-mod=` args from
    /// the sources its spec references, without re-registering anything).
    pub async fn get_source_mods(&self, source_id: &str) -> anyhow::Result<Vec<u64>> {
        let response = self
            .inner
            .get_source_mods(GetSourceModsRequest {
                source_id: source_id.to_string(),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("GetSourceMods failed: {e}"))?;
        Ok(response.view().mod_ids.to_vec())
    }

    /// Force `source_id` to be re-resolved against its originally-registered
    /// candidate IDs -- used by `ServerService::UpdateServer`/`StartServer`.
    pub async fn refresh_source(&self, source_id: &str) -> anyhow::Result<()> {
        self.inner
            .refresh_source(RefreshSourceRequest {
                source_id: source_id.to_string(),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("RefreshSource failed: {e}"))?;
        Ok(())
    }

    /// Explicit, deliberate removal of a source -- called from
    /// `ModSourceService::DeleteModSource`, never from server deletion (see
    /// that RPC's doc comment: a server going away must not silently stop
    /// syncing mods someone may want kept warm).
    pub async fn deregister_source(&self, source_id: &str) -> anyhow::Result<()> {
        self.inner
            .deregister_source(DeregisterSourceRequest {
                source_id: source_id.to_string(),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("DeregisterSource failed: {e}"))?;
        Ok(())
    }

    /// Kick off a claim job covering the full current desired state (every
    /// registered source, not just this one) and return its job ID.
    pub async fn claim(&self) -> anyhow::Result<String> {
        let response = self
            .inner
            .claim(ClaimRequest::default())
            .await
            .map_err(|e| anyhow::anyhow!("Claim failed: {e}"))?;
        Ok(response.view().job_id.to_string())
    }

    /// Every currently-tracked workshop mod ID and the manifest_id it was
    /// last verified at.
    pub async fn list_synced_mods(&self) -> anyhow::Result<Vec<SyncedMod>> {
        let response = self
            .inner
            .list_synced_mods(ListSyncedModsRequest::default())
            .await
            .map_err(|e| anyhow::anyhow!("ListSyncedMods failed: {e}"))?;
        Ok(response
            .view()
            .mods
            .iter()
            .map(|m| SyncedMod {
                mod_id: m.mod_id,
                manifest_id: m.manifest_id,
                size_bytes: m.size_bytes,
                title: m.title.to_string(),
            })
            .collect())
    }

    /// A single mod's synced state (`None` if not currently tracked as
    /// synced) plus every source_id currently referencing it.
    pub async fn get_synced_mod(
        &self,
        mod_id: u64,
    ) -> anyhow::Result<(Option<SyncedMod>, Vec<String>)> {
        let response = self
            .inner
            .get_synced_mod(GetSyncedModRequest {
                mod_id,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("GetSyncedMod failed: {e}"))?;
        let view = response.view();
        let m = view.r#mod.as_option().map(|m| SyncedMod {
            mod_id: m.mod_id,
            manifest_id: m.manifest_id,
            size_bytes: m.size_bytes,
            title: m.title.to_string(),
        });
        Ok((m, view.source_ids.iter().map(|s| s.to_string()).collect()))
    }

    /// Clear one mod's "last verified" marker -- never deletes its files.
    /// The next resolve pass genuinely re-verifies it against Steam's
    /// current manifest, redownloading only whatever's missing/divergent.
    pub async fn invalidate_mod(&self, mod_id: u64) -> anyhow::Result<()> {
        self.inner
            .invalidate_mod(InvalidateModRequest {
                mod_id,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("InvalidateMod failed: {e}"))?;
        Ok(())
    }

    /// Cluster-wide totals: every synced mod's size deduplicated across
    /// sources, plus base game/CDLC depot size.
    pub async fn sync_stats(&self) -> anyhow::Result<SyncStats> {
        let response = self
            .inner
            .get_sync_stats(GetSyncStatsRequest::default())
            .await
            .map_err(|e| anyhow::anyhow!("GetSyncStats failed: {e}"))?;
        let view = response.view();
        Ok(SyncStats {
            mods_bytes: view.mods_bytes,
            game_files_bytes: view.game_files_bytes,
        })
    }

    /// Establish (or replace) the Steam session interactively -- proxies
    /// straight through to sync-daemon's own RPC of the same name.
    pub async fn refresh_steam_auth(
        &self,
        username: &str,
        password: &str,
        guard_code: Option<&str>,
    ) -> anyhow::Result<RefreshSteamAuthResult> {
        let response = self
            .inner
            .refresh_steam_auth(RefreshSteamAuthRequest {
                username: username.to_string(),
                password: password.to_string(),
                guard_code: guard_code.map(str::to_string),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("RefreshSteamAuth failed: {e}"))?;
        let view = response.view();
        Ok(if view.needs_guard {
            RefreshSteamAuthResult::NeedsGuard {
                guard_type: view.guard_type.to_string(),
            }
        } else {
            RefreshSteamAuthResult::Success
        })
    }

    pub async fn claim_status(&self, job_id: &str) -> anyhow::Result<ClaimStatus> {
        let response = self
            .inner
            .get_claim_status(GetClaimStatusRequest {
                job_id: job_id.to_string(),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("GetClaimStatus failed: {e}"))?;
        let view = response.view();
        let status = match view.state {
            buffa::enumeration::EnumValue::Known(ClaimJobState::Done) => ClaimStatus::Done {
                claim_path: view.claim_path.to_string(),
            },
            buffa::enumeration::EnumValue::Known(ClaimJobState::Failed) => ClaimStatus::Failed {
                error: view.error.to_string(),
            },
            _ => ClaimStatus::Running,
        };
        Ok(status)
    }
}
