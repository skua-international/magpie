use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use protocol::proto::volume::v1::{
    GetVolumeStatusRequest, GetVolumeStatusResponse, GrowVolumeRequest, GrowVolumeResponse,
    VolumeManager,
};

use crate::blob::BlobManager;

pub struct VolumeManagerImpl {
    blob: BlobManager,
}

impl VolumeManagerImpl {
    pub fn new(blob: BlobManager) -> Self {
        Self { blob }
    }

    /// See BlobManager::is_ready -- exposed here so main.rs's /healthz
    /// handler doesn't need its own separate handle on the BlobManager.
    pub async fn is_ready(&self) -> bool {
        self.blob.is_ready().await
    }
}

impl VolumeManager for VolumeManagerImpl {
    async fn grow_volume<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GrowVolumeRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GrowVolumeResponse> + Send + use<'a>> {
        let outcome = self
            .blob
            .ensure_capacity(request.bytes_needed)
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(GrowVolumeResponse {
            total_bytes: outcome.total_bytes,
            free_bytes: outcome.free_bytes,
            grew: outcome.grew,
            ..Default::default()
        })
    }

    async fn get_volume_status<'a>(
        &'a self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetVolumeStatusRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<GetVolumeStatusResponse> + Send + use<'a>> {
        let (total_bytes, free_bytes) = self
            .blob
            .status()
            .await
            .map_err(|e| ConnectError::internal(format!("{e:#}")))?;
        Response::ok(GetVolumeStatusResponse {
            total_bytes,
            free_bytes,
            ..Default::default()
        })
    }
}
