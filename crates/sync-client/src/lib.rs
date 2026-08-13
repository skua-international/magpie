//! Thin wrapper around a generated Connect client for sync-daemon's
//! `SyncService`, used only by the reconciler -- the controller never talks
//! to Steam itself, only in terms of candidate/resolved mod IDs.

use connectrpc::client::{ClientConfig, HttpClient};
use protocol::proto::sync::v1::{
    BeginQrLoginRequest, DeregisterSourceRequest, GetSourceModsRequest, GetSyncStatsRequest,
    GetSyncStatusRequest, GetSyncedModRequest, InvalidateModRequest, ListSyncedModsRequest,
    PollQrLoginRequest, RefreshSourceRequest, RefreshSteamAuthRequest, RegisterSourceRequest,
    SyncContentRequest, SyncServiceClient,
};

pub struct SyncClient {
    inner: SyncServiceClient<HttpClient>,
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

pub struct SyncStatus {
    pub syncing: bool,
    pub game_files_ready: bool,
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

    /// Fire-and-forget: tells sync-daemon to sync server/CDLC depots plus
    /// every registered source's resolved mods into the golden content
    /// tree. Doesn't wait for completion -- see sync.proto's own doc on
    /// `SyncContent` for why. Each ArmaServer's own content comes from a
    /// PVC backed by a read-only btrfs snapshot of that tree instead
    /// (services/magpie-csi), created/deleted by Kubernetes' normal PVC
    /// lifecycle -- not this crate's concern.
    pub async fn sync_content(&self) -> anyhow::Result<()> {
        self.inner
            .sync_content(SyncContentRequest::default())
            .await
            .map_err(|e| anyhow::anyhow!("SyncContent failed: {e}"))?;
        Ok(())
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

    /// Whether the golden content tree is safe to snapshot from right now
    /// -- the reconciler calls this before creating an ArmaServer's
    /// Deployment, so a launcher Pod never gets a CSI snapshot of a
    /// base-game/CDLC sync that's still mid-download. See the proto's own
    /// doc for why this exists as its own call rather than being folded
    /// into `sync_stats`.
    pub async fn sync_status(&self) -> anyhow::Result<SyncStatus> {
        let response = self
            .inner
            .get_sync_status(GetSyncStatusRequest::default())
            .await
            .map_err(|e| anyhow::anyhow!("GetSyncStatus failed: {e}"))?;
        let view = response.view();
        Ok(SyncStatus {
            syncing: view.syncing,
            game_files_ready: view.game_files_ready,
        })
    }

    /// Begin a QR login on sync-daemon and return `(session_id,
    /// challenge_url)`. The Steam CM connection this opens lives there,
    /// not here -- see the RPC's own proto doc.
    pub async fn begin_qr_login(&self) -> anyhow::Result<(String, String)> {
        let resp = self
            .inner
            .begin_qr_login(BeginQrLoginRequest::default())
            .await
            .map_err(|e| anyhow::anyhow!("BeginQrLogin failed: {e}"))?;
        let view = resp.view();
        Ok((view.session_id.to_string(), view.challenge_url.to_string()))
    }

    /// Current state of a QR login: `(confirmed, username)`. Returns
    /// immediately -- the blocking wait happens in sync-daemon.
    pub async fn poll_qr_login(&self, session_id: &str) -> anyhow::Result<(bool, String)> {
        let resp = self
            .inner
            .poll_qr_login(PollQrLoginRequest {
                session_id: session_id.to_string(),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("PollQrLogin failed: {e}"))?;
        let view = resp.view();
        Ok((view.confirmed, view.username.to_string()))
    }

    /// Establish (or replace) the Steam session interactively -- proxies
    /// straight through to sync-daemon's own RPC of the same name.
    /// `refresh_token` must already be negotiated -- this process (and
    /// everything downstream of it) never handles a Steam password, see
    /// the RPC's own proto doc for the full rationale.
    pub async fn refresh_steam_auth(
        &self,
        username: &str,
        refresh_token: &str,
    ) -> anyhow::Result<()> {
        self.inner
            .refresh_steam_auth(RefreshSteamAuthRequest {
                username: username.to_string(),
                refresh_token: refresh_token.to_string(),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("RefreshSteamAuth failed: {e}"))?;
        Ok(())
    }
}
