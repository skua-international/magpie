//! Connect client for magpie-csi's `csi.v1.CapacityService`.
//!
//! Implements steam-sync's `CapacityReserver` so the download paths can
//! announce a batch's size before dispatching it, instead of leaving
//! magpie-csi's watchdog to notice free space dropping after the fact.
//!
//! Addressed per-node, not through a ClusterIP Service: the blob is
//! node-local, so the only correct target is the CSI Node plugin running
//! on this Pod's own node. See the chart's CSI_CAPACITY_URL, built from
//! a status.hostIP fieldRef.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use connectrpc::client::{ClientConfig, HttpClient};
use protocol::proto::csi::v1::{
    CapacityServiceClient, ReleaseCapacityRequest, ReserveCapacityRequest,
};
use steam_sync::capacity::CapacityReserver;
use tracing::{info, warn};

/// How long a reservation is held before magpie-csi expires it on its
/// own. Generous relative to a batch: a cold full-content sync is tens
/// of minutes on a slow link, and expiring mid-download puts back
/// exactly the race this exists to close. The far side clamps this to
/// its own ceiling, so an over-long value here is safe.
const RESERVATION_TTL: Duration = Duration::from_secs(4 * 60 * 60);

/// Bounds how long a batch waits on the reservation round trip. Growing
/// the blob is a sparse truncate plus an online btrfs resize -- fast --
/// so anything approaching this means magpie-csi is unreachable or
/// wedged, and waiting longer just delays a sync that would very likely
/// have succeeded anyway.
const RESERVE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct CapacityClient {
    inner: CapacityServiceClient<HttpClient>,
}

impl CapacityClient {
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        let uri: http::Uri = base_url.parse()?;
        let transport = HttpClient::plaintext();
        let config = ClientConfig::new(uri);
        Ok(Self {
            inner: CapacityServiceClient::new(transport, config),
        })
    }
}

impl CapacityReserver for CapacityClient {
    fn reserve<'a>(
        &'a self,
        key: &'a str,
        bytes: u64,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let req = ReserveCapacityRequest {
                key: key.to_string(),
                bytes,
                ttl_seconds: RESERVATION_TTL.as_secs() as u32,
                ..Default::default()
            };

            // Every failure below is a warning, never an error return.
            // magpie-csi's capacity watchdog is still running regardless,
            // so a missed reservation degrades to exactly the behavior
            // that shipped before this existed -- whereas failing the
            // sync would turn a rare, recoverable problem into a certain
            // one. See CapacityReserver's own doc.
            match tokio::time::timeout(RESERVE_TIMEOUT, self.inner.reserve_capacity(req)).await {
                Ok(Ok(resp)) => {
                    let resp = resp.view();
                    if resp.satisfied {
                        info!(
                            "reserved {bytes} bytes for {key} (blob now {} bytes, {} free)",
                            resp.total_bytes, resp.free_bytes
                        );
                    } else {
                        warn!(
                            "capacity reservation for {key} ({bytes} bytes) was not satisfied -- blob is {} bytes with {} free, likely capped by reflinkStorage.maxSizeGiB; proceeding anyway",
                            resp.total_bytes, resp.free_bytes
                        );
                    }
                }
                Ok(Err(e)) => {
                    warn!(
                        "capacity reservation for {key} ({bytes} bytes) failed: {e:#}; proceeding, magpie-csi's watchdog is the fallback"
                    );
                }
                Err(_) => {
                    warn!(
                        "capacity reservation for {key} ({bytes} bytes) timed out after {RESERVE_TIMEOUT:?}; proceeding, magpie-csi's watchdog is the fallback"
                    );
                }
            }
        })
    }

    fn release<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let req = ReleaseCapacityRequest {
                key: key.to_string(),
                ..Default::default()
            };
            if let Err(e) = self.inner.release_capacity(req).await {
                // Genuinely harmless: the reservation's TTL expires it
                // regardless, this only returns the space sooner.
                warn!("failed to release capacity reservation {key}: {e:#}");
            }
        })
    }
}
