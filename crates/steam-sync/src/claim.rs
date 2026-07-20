//! Read-only btrfs snapshot "claims" of a fully-synced content tree.
//!
//! More than one server instance can share one golden, continuously-synced
//! content directory. Rather than each instance running directly against
//! that shared tree, a claim gives an instance its own private, isolated
//! view to run against -- a `btrfs subvolume snapshot -r`, one atomic
//! ioctl regardless of how many files the tree contains, sharing extents
//! with the golden tree until something actually diverges. `golden_dir`
//! must already be a real btrfs subvolume for this to work (magpie-csi's
//! blob.go creates it as one at blob-bootstrap time, not this crate's
//! job) -- this crate does no filesystem-portability fallback the way an
//! earlier `cp --reflink=always`-based version of this did, since
//! magpie-csi unconditionally formats its own blob as btrfs regardless of
//! the host's own filesystem, so there's never actually a non-btrfs
//! target to degrade for in this deployment.
//!
//! Read-only, not read-write: nothing ever writes back into a claim after
//! creation (launcher Pods mount their claim subPath read-only at the
//! Kubernetes level too, see reconcile.rs), so there's no reason to pay
//! for a writable snapshot's extra bookkeeping.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Snapshots `golden_dir` (a btrfs subvolume) into `claim_dir`. `claim_dir`
/// must not already exist -- claims are meant to be fresh, disposable
/// copies, not merged into existing state.
pub fn claim(golden_dir: &Path, claim_dir: &Path) -> Result<PathBuf> {
    if claim_dir.exists() {
        bail!("claim dir {} already exists", claim_dir.display());
    }
    if let Some(parent) = claim_dir.parent() {
        std::fs::create_dir_all(parent).context("failed to create claim parent dir")?;
    }

    run_btrfs(&["subvolume", "snapshot", "-r"], golden_dir, claim_dir).with_context(|| {
        format!(
            "failed to snapshot {} -> {}",
            golden_dir.display(),
            claim_dir.display()
        )
    })?;
    tracing::debug!(
        "claimed {} -> {} (btrfs snapshot)",
        golden_dir.display(),
        claim_dir.display()
    );

    Ok(claim_dir.to_path_buf())
}

/// Releases a claim -- `btrfs subvolume delete`, not `rm -rf`: a claim
/// snapshot is a real subvolume, and plain directory removal leaves a
/// subvolume dangling (invisible to normal file tools, still consuming
/// its own space) rather than actually freeing it. A no-op (not an
/// error) if `claim_dir` is already gone -- delete-on-exit call sites
/// (see services/launcher) shouldn't fail just because something else
/// already cleaned this claim up first.
pub fn delete_claim(claim_dir: &Path) -> Result<()> {
    if !claim_dir.exists() {
        return Ok(());
    }
    let status = std::process::Command::new("btrfs")
        .arg("subvolume")
        .arg("delete")
        .arg(claim_dir)
        .status()
        .context("failed to spawn btrfs subvolume delete")?;
    if !status.success() {
        bail!(
            "btrfs subvolume delete {} exited with {status}",
            claim_dir.display()
        );
    }
    Ok(())
}

fn run_btrfs(args: &[&str], from: &Path, to: &Path) -> Result<()> {
    let status = std::process::Command::new("btrfs")
        .args(args)
        .arg(from)
        .arg(to)
        .status()
        .context("failed to spawn btrfs")?;
    if !status.success() {
        bail!("btrfs {} exited with {status}", args.join(" "));
    }
    Ok(())
}
