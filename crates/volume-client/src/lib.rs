//! Thin wrapper around a generated Connect client for volume-manager's
//! `VolumeManager` service -- used only by sync-daemon, the sole caller
//! this service's NetworkPolicy allows through at all. See
//! proto/volume/v1/volume.proto for why this exists (blob-backed btrfs
//! for reflink, growth needs host block-device privilege sync-daemon
//! itself deliberately doesn't hold).

use connectrpc::client::{ClientConfig, HttpClient};
use protocol::proto::volume::v1::{GetVolumeStatusRequest, GrowVolumeRequest, VolumeManagerClient};

pub struct VolumeClient {
    inner: VolumeManagerClient<HttpClient>,
}

pub struct GrowResult {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub grew: bool,
}

pub struct VolumeStatus {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl VolumeClient {
    /// `token` is the shared bearer secret both this pod and
    /// volume-manager's Secret carry -- see charts/magpie's
    /// volume-manager-secret.yaml. Attached as a default header so every
    /// call through this client is authenticated, not just some of them.
    pub fn new(base_url: &str, token: &str) -> anyhow::Result<Self> {
        let uri: http::Uri = base_url.parse()?;
        let transport = HttpClient::plaintext();
        let config = ClientConfig::new(uri)
            .with_default_header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        Ok(Self {
            inner: VolumeManagerClient::new(transport, config),
        })
    }

    /// Ensure at least `bytes_needed` free space exists, growing (or
    /// first-time bootstrapping) the managed filesystem if not. Idempotent.
    pub async fn grow_volume(&self, bytes_needed: u64) -> anyhow::Result<GrowResult> {
        let response = self
            .inner
            .grow_volume(GrowVolumeRequest {
                bytes_needed,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("GrowVolume failed: {e}"))?;
        let view = response.view();
        Ok(GrowResult {
            total_bytes: view.total_bytes,
            free_bytes: view.free_bytes,
            grew: view.grew,
        })
    }

    pub async fn status(&self) -> anyhow::Result<VolumeStatus> {
        let response = self
            .inner
            .get_volume_status(GetVolumeStatusRequest::default())
            .await
            .map_err(|e| anyhow::anyhow!("GetVolumeStatus failed: {e}"))?;
        let view = response.view();
        Ok(VolumeStatus {
            total_bytes: view.total_bytes,
            free_bytes: view.free_bytes,
        })
    }
}
