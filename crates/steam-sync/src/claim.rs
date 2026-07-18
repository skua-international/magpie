//! Copy-on-write "claims" of a fully-synced content tree.
//!
//! More than one server instance can share one golden, continuously-synced
//! content directory. Rather than each instance running (and writing)
//! directly against that shared tree, a claim gives an instance its own
//! private copy to run against -- cheap when the underlying filesystem
//! supports reflink (btrfs, XFS with reflink=1): a `claim()` call copies
//! metadata, not bytes, so it's fast and doesn't multiply disk usage per
//! instance. On a filesystem without reflink support, this degrades safely
//! to a real copy -- slower and disk-hungry, but still correct.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Reflink-copy (falling back to a real copy) `golden_dir` into `claim_dir`.
/// `claim_dir` must not already exist -- claims are meant to be fresh,
/// disposable copies, not merged into existing state.
pub fn claim(golden_dir: &Path, claim_dir: &Path) -> Result<PathBuf> {
    if claim_dir.exists() {
        bail!("claim dir {} already exists", claim_dir.display());
    }
    if let Some(parent) = claim_dir.parent() {
        std::fs::create_dir_all(parent).context("failed to create claim parent dir")?;
    }

    match run_cp_reflink(golden_dir, claim_dir) {
        Ok(()) => {
            tracing::debug!(
                "claimed {} -> {} (reflink)",
                golden_dir.display(),
                claim_dir.display()
            );
        }
        Err(e) => {
            tracing::warn!(
                "reflink copy failed ({e:#}), falling back to a real copy -- this will be slower \
                 and use real disk space proportional to the content size"
            );
            copy_dir_recursive(golden_dir, claim_dir).with_context(|| {
                format!(
                    "failed to copy {} -> {}",
                    golden_dir.display(),
                    claim_dir.display()
                )
            })?;
        }
    }

    Ok(claim_dir.to_path_buf())
}

/// Shell out to `cp --reflink=always -r`, which understands both btrfs and
/// XFS reflink semantics without us needing filesystem-specific ioctl
/// bindings for each. Fails outright (rather than silently falling back
/// itself) if the filesystem doesn't support reflink, so the caller can
/// decide how to handle that -- here, one real recursive copy.
fn run_cp_reflink(from: &Path, to: &Path) -> Result<()> {
    let status = std::process::Command::new("cp")
        .arg("--reflink=always")
        .arg("-r")
        .arg(from)
        .arg(to)
        .status()
        .context("failed to spawn cp")?;
    if !status.success() {
        // Reflink copy leaves no partial directory behind on failure in
        // practice (cp aborts before creating anything on an unsupported
        // fs), but clean up defensively before the fallback copy runs.
        let _ = std::fs::remove_dir_all(to);
        bail!("cp --reflink=always exited with {status}");
    }
    Ok(())
}

fn copy_dir_recursive(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}
