use std::env;
use std::path::PathBuf;

use anyhow::Result;

pub struct Config {
    pub listen_addr: String,
    /// The blob backing file itself (e.g. a hostPath-mounted
    /// /blob/content.img) -- lives outside blob_mount_path, since a
    /// filesystem's own backing file obviously can't live inside the
    /// filesystem it backs.
    pub blob_image_path: PathBuf,
    /// Where the blob gets mounted -- content/claims (sync-daemon's
    /// hostPaths.contentPath/claimsPath) live as subdirectories under
    /// here once mounted.
    pub blob_mount_path: PathBuf,
    /// Used only the very first time GrowVolume is ever called (the blob
    /// image doesn't exist yet) -- every call after that only grows from
    /// the filesystem's actual current size, this never shrinks or resets
    /// anything.
    pub initial_size_bytes: u64,
    /// Compared against the caller's Authorization: Bearer header --
    /// see charts/magpie's volume-manager-secret.yaml. sync-daemon is the
    /// only caller this service's NetworkPolicy allows through at all,
    /// but the token check still matters as defense in depth.
    pub auth_token: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8446".into()),
            blob_image_path: env::var("BLOB_IMAGE_PATH")
                .unwrap_or_else(|_| "/blob/content.img".into())
                .into(),
            blob_mount_path: env::var("BLOB_MOUNT_PATH")
                .unwrap_or_else(|_| "/mnt/blob".into())
                .into(),
            initial_size_bytes: env::var("INITIAL_SIZE_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(64 * 1024 * 1024 * 1024), // 64GiB, overridden by the chart
            auth_token: env::var("AUTH_TOKEN")
                .map_err(|_| anyhow::anyhow!("AUTH_TOKEN must be set"))?,
        })
    }
}
