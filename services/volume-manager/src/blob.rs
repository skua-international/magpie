//! Owns the actual privileged host operations: creating/growing a loop-
//! mounted btrfs blob so reflink CoW claims work regardless of what
//! filesystem the host actually has. Shells out to standard tools
//! (truncate, losetup, mkfs.btrfs, mount, btrfs, df) rather than binding
//! raw ioctls directly -- same convention crates/steam-sync's own claim()
//! already established for `cp --reflink=always` (see its doc comment).
//!
//! All mutating operations go through `ensure_capacity`, which holds an
//! internal lock for its whole duration -- GrowVolume calls never race
//! each other, even under concurrent RPCs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::sync::Mutex;

pub struct BlobManager {
    image_path: PathBuf,
    mount_path: PathBuf,
    initial_size_bytes: u64,
    lock: Mutex<()>,
}

pub struct GrowOutcome {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub grew: bool,
}

impl BlobManager {
    pub fn new(image_path: PathBuf, mount_path: PathBuf, initial_size_bytes: u64) -> Self {
        Self {
            image_path,
            mount_path,
            initial_size_bytes,
            lock: Mutex::new(()),
        }
    }

    /// Ensures `bytes_needed` free space exists, growing (or first-time
    /// bootstrapping) if not. Idempotent -- safe to call even when nothing
    /// actually needs to happen.
    pub async fn ensure_capacity(&self, bytes_needed: u64) -> Result<GrowOutcome> {
        let _guard = self.lock.lock().await;

        if !self.is_mounted().await? {
            self.bootstrap(bytes_needed.max(self.initial_size_bytes))
                .await
                .context("failed to bootstrap blob filesystem")?;
            let (total_bytes, free_bytes) = self.statvfs().await?;
            return Ok(GrowOutcome {
                total_bytes,
                free_bytes,
                grew: true,
            });
        }

        let (total_bytes, free_bytes) = self.statvfs().await?;
        if free_bytes >= bytes_needed {
            return Ok(GrowOutcome {
                total_bytes,
                free_bytes,
                grew: false,
            });
        }

        let target_total = total_bytes + (bytes_needed - free_bytes);
        self.grow_to(target_total)
            .await
            .context("failed to grow blob filesystem")?;
        let (total_bytes, free_bytes) = self.statvfs().await?;
        Ok(GrowOutcome {
            total_bytes,
            free_bytes,
            grew: true,
        })
    }

    pub async fn status(&self) -> Result<(u64, u64)> {
        let _guard = self.lock.lock().await;
        if !self.is_mounted().await? {
            return Ok((0, 0));
        }
        self.statvfs().await
    }

    async fn is_mounted(&self) -> Result<bool> {
        let mount_str = self.mount_path.to_string_lossy();
        let out = Command::new("findmnt")
            .args(["--noheadings", "--target", &mount_str])
            .output()
            .await
            .context("failed to run findmnt")?;
        Ok(out.status.success())
    }

    /// First-time setup: creates the backing file if it doesn't exist yet
    /// (never re-creates or re-formats an image that's already there --
    /// this path also runs on every restart until the mount succeeds, so
    /// it has to be safe to call against a real, populated image after a
    /// host reboot dropped the loop attachment/mount).
    async fn bootstrap(&self, min_size_bytes: u64) -> Result<()> {
        let is_new_image = !self.image_path.exists();

        if let Some(parent) = self.image_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        if is_new_image {
            run(
                "truncate",
                &[
                    "-s",
                    &min_size_bytes.to_string(),
                    &path_str(&self.image_path),
                ],
            )
            .await?;
        }

        let loop_dev = self.attach_loop_device().await?;

        if is_new_image {
            run("mkfs.btrfs", &["-f", &loop_dev]).await?;
        }

        tokio::fs::create_dir_all(&self.mount_path)
            .await
            .with_context(|| format!("failed to create {}", self.mount_path.display()))?;
        run("mount", &[&loop_dev, &path_str(&self.mount_path)]).await?;

        Ok(())
    }

    async fn grow_to(&self, new_size_bytes: u64) -> Result<()> {
        run(
            "truncate",
            &[
                "-s",
                &new_size_bytes.to_string(),
                &path_str(&self.image_path),
            ],
        )
        .await?;
        let loop_dev = self.attach_loop_device().await?;
        // Tells the kernel to re-read the backing file's now-larger size.
        run("losetup", &["-c", &loop_dev]).await?;
        run(
            "btrfs",
            &["filesystem", "resize", "max", &path_str(&self.mount_path)],
        )
        .await?;
        Ok(())
    }

    /// Idempotent: reuses an existing loop association for this image if
    /// one's already there (the common case after a restart) instead of
    /// attaching a second one.
    async fn attach_loop_device(&self) -> Result<String> {
        let image = path_str(&self.image_path);

        let out = Command::new("losetup")
            .args(["-j", &image])
            .output()
            .await?;
        let listing = String::from_utf8_lossy(&out.stdout);
        if let Some(dev) = listing.split(':').next().filter(|s| !s.is_empty()) {
            return Ok(dev.trim().to_string());
        }

        let out = Command::new("losetup")
            .args(["-f", "--show", &image])
            .output()
            .await
            .context("failed to run losetup -f")?;
        if !out.status.success() {
            bail!(
                "losetup -f --show {image} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// (total_bytes, free_bytes) via `df` -- shelling out rather than
    /// binding statvfs(2) directly, same reasoning as everything else in
    /// this file.
    async fn statvfs(&self) -> Result<(u64, u64)> {
        let out = Command::new("df")
            .args([
                "--output=size,avail",
                "--block-size=1",
                &path_str(&self.mount_path),
            ])
            .output()
            .await
            .context("failed to run df")?;
        if !out.status.success() {
            bail!(
                "df {} failed: {}",
                self.mount_path.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let data_line = text
            .lines()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("unexpected df output: {text}"))?;
        let mut fields = data_line.split_whitespace();
        let total: u64 = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("unexpected df output: {text}"))?
            .parse()?;
        let free: u64 = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("unexpected df output: {text}"))?
            .parse()?;
        Ok((total, free))
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

async fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to run {cmd}"))?;
    if !out.status.success() {
        bail!(
            "{cmd} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}
