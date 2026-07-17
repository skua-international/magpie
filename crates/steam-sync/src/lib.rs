//! Steam login, depot/workshop resolution, and cross-process-safe sync
//! state/locking for downloading Arma 3 server + workshop content.
//!
//! Extracted from the launcher's original `steam.rs`/`cache.rs`/`workshop.rs`
//! so a future sync daemon can depend on the same resolve/sync/lock logic
//! without depending on the launcher (game-launch) binary itself.

pub mod cache;
pub mod claim;
pub mod steam;
pub mod workshop;
