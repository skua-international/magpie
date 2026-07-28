//! Announcing an upcoming batch's size before downloading it.
//!
//! Depot downloads run up to `SYNC_CONCURRENCY` (128) wide, so a large
//! preset can put well over a hundred concurrent writers on the content
//! tree. Whatever is managing that filesystem's size cannot learn about
//! that from free-space polling in time -- confirmed live on 2026-07-26,
//! a startup sync blew through 5GiB of headroom inside the poll gap. But
//! the size *is* knowable here: manifests are fetched before any chunk
//! is, and each carries its depot's total on-disk bytes.
//!
//! This trait is the seam for saying so. steam-sync stays ignorant of
//! who's listening or how they're reached -- sync-daemon supplies the
//! implementation (a Connect client to magpie-csi).

use std::future::Future;
use std::pin::Pin;

/// Somewhere to announce "about to write N bytes" before writing them.
///
/// `reserve` returns no error on purpose. A reservation is an
/// optimization over the consumer's own fallback (magpie-csi keeps its
/// capacity watchdog regardless), so failing a sync because the
/// announcement didn't land would trade a rare, recoverable problem for
/// a certain one. Implementations log their own failures and return.
pub trait CapacityReserver: Send + Sync {
    /// `key` scopes the reservation so re-reserving replaces rather than
    /// accumulates; distinct keys sum. Returns once the space is
    /// actually available, so callers can await this to gate dispatch.
    fn reserve<'a>(
        &'a self,
        key: &'a str,
        bytes: u64,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Drop a reservation once its batch is done. Best-effort -- every
    /// reservation expires on its own TTL anyway.
    fn release<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Reservation key for the base game + CDLC depot batch.
pub const KEY_SERVER_DEPOTS: &str = "server-depots";

/// Reservation key for the workshop mod batch.
pub const KEY_WORKSHOP: &str = "workshop";

/// Sum the on-disk bytes a set of resolved depot plans will occupy.
///
/// An upper bound, deliberately: `cb_disk_original` is the depot's full
/// size, and `sync_depot` verifies existing chunks first and only
/// downloads what diverges, so a depot already 95% present still counts
/// in full here. Over-reserving is the safe direction -- the cost is
/// briefly holding more room than needed, versus running out mid-write.
///
/// Plans whose manifest never resolved contribute nothing; they aren't
/// going to be dispatched either.
pub fn total_disk_bytes<'a>(
    plans: impl IntoIterator<Item = &'a steamdepot::depot::DepotPlan>,
) -> u64 {
    plans
        .into_iter()
        .filter_map(|dp| dp.manifest.as_ref())
        .map(|m| m.metadata.cb_disk_original.unwrap_or(0))
        .sum()
}
