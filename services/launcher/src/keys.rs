use std::path::Path;

use anyhow::{Context, Result};
use walkdir::WalkDir;

const KEYS_DIR: &str = "/arma3/server/keys";

/// Recreate the keys directory when `CLEAR_KEYS=true`, otherwise just make
/// sure it exists (matches launch.py's top-of-file setup).
pub fn ensure_keys_dir() -> Result<()> {
    let dir = Path::new(KEYS_DIR);
    let clear = std::env::var("CLEAR_KEYS")
        .map(|v| v == "true")
        .unwrap_or(false);

    if clear && dir.is_dir() {
        std::fs::remove_dir_all(dir).context("failed to clear keys dir")?;
    }
    if !dir.is_dir() {
        if dir.exists() {
            std::fs::remove_file(dir).context("failed to remove non-directory at keys path")?;
        }
        std::fs::create_dir_all(dir).context("failed to create keys dir")?;
    }
    Ok(())
}

/// Copy every `.bikey` found under `mod_dir` into the server's keys directory.
pub fn copy(mod_dir: &Path) -> Result<()> {
    let mut found = 0;

    for entry in WalkDir::new(mod_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|e| e.to_str()) != Some("bikey") {
            continue;
        }

        let dest = Path::new(KEYS_DIR).join(entry.file_name());
        std::fs::copy(entry.path(), &dest).with_context(|| {
            format!(
                "failed to copy key {} -> {}",
                entry.path().display(),
                dest.display()
            )
        })?;
        found += 1;
    }

    if found == 0 {
        tracing::warn!("Missing keys: {}", mod_dir.display());
    }

    Ok(())
}
