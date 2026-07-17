//! Blob storage for local (zip-uploaded) mods and missions, on a shared
//! volume mounted into the controller and, read-only, into every launcher
//! Pod (see `reconcile.rs`). Deliberately outside sync-daemon/steam-sync
//! entirely -- neither of these content types has anything to do with
//! Steam, so they don't belong in the reflink-claim/manifest-tracking
//! machinery that exists specifically to handle Steam depot updates.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use uuid::Uuid;

pub fn local_mods_dir(root: &Path) -> PathBuf {
    root.join("mods")
}

pub fn missions_dir(root: &Path) -> PathBuf {
    root.join("missions")
}

/// Extracts a zip archive into `<root>/mods/<unique_id>/`, flattening a
/// single top-level wrapper folder if present (a common zip-export shape)
/// so `mod.cpp` always ends up directly inside the mod's own directory --
/// required for it to work as a `-mod=` path.
pub fn extract_local_mod(root: &Path, unique_id: &str, zip_bytes: &[u8]) -> Result<PathBuf> {
    validate_path_component(unique_id)?;
    let dest = local_mods_dir(root).join(unique_id);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).context("failed to remove existing local mod directory before re-extracting")?;
    }
    std::fs::create_dir_all(&dest).context("failed to create local mod directory")?;

    let reader = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(reader).context("failed to read zip archive")?;
    archive.extract(&dest).context("failed to extract zip archive")?;

    let mod_root = if dest.join("mod.cpp").is_file() {
        dest.clone()
    } else {
        // Look for a single top-level directory wrapping the actual mod
        // content (e.g. the zip was exported as "MyMod-main/mod.cpp").
        let entries: Vec<_> = std::fs::read_dir(&dest).context("failed to list extracted mod directory")?.filter_map(|e| e.ok()).collect();
        match entries.as_slice() {
            [entry] if entry.path().is_dir() && entry.path().join("mod.cpp").is_file() => entry.path(),
            _ => bail!("extracted archive has no mod.cpp at its root or in a single wrapping folder"),
        }
    };

    if mod_root != dest {
        // Flatten: move the wrapper folder's contents up into `dest` itself
        // so the mod's on-disk path is always exactly `dest`, regardless of
        // how the uploaded zip was packaged.
        for entry in std::fs::read_dir(&mod_root)? {
            let entry = entry?;
            let target = dest.join(entry.file_name());
            std::fs::rename(entry.path(), target)?;
        }
        std::fs::remove_dir(&mod_root).context("failed to remove now-empty wrapper folder")?;
    }

    Ok(dest)
}

pub fn delete_local_mod(root: &Path, unique_id: &str) -> Result<()> {
    validate_path_component(unique_id)?;
    let dest = local_mods_dir(root).join(unique_id);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).context("failed to delete local mod directory")?;
    }
    Ok(())
}

/// Stored as `<uuid>__<sanitized name>` -- the UUID keeps the filename
/// stable and collision-free across re-uploads (a mission can be
/// overwritten with a different original filename), while keeping the
/// human-meaningful name in the actual file on disk, since Arma's mission
/// browser and mission-selection config reference missions by filename.
fn mission_filename(id: Uuid, name: &str) -> String {
    let sanitized: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    format!("{id}__{sanitized}")
}

pub fn write_mission(root: &Path, id: Uuid, name: &str, pbo_bytes: &[u8]) -> Result<PathBuf> {
    let dir = missions_dir(root);
    std::fs::create_dir_all(&dir).context("failed to create missions directory")?;

    // A re-upload (overwrite) may change the filename -- remove whatever
    // this UUID previously wrote before writing the new one.
    delete_mission_file(root, id)?;

    let path = dir.join(mission_filename(id, name));
    std::fs::write(&path, pbo_bytes).context("failed to write mission file")?;
    Ok(path)
}

pub fn delete_mission_file(root: &Path, id: Uuid) -> Result<()> {
    let dir = missions_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(()) };
    let prefix = format!("{id}__");
    for entry in entries.filter_map(|e| e.ok()) {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            std::fs::remove_file(entry.path()).context("failed to delete mission file")?;
        }
    }
    Ok(())
}

/// Guards against a `unique_id` containing path traversal (`..`, `/`) --
/// it's caller-assigned and used directly as a directory name.
fn validate_path_component(s: &str) -> Result<()> {
    if s.is_empty() || s.contains('/') || s.contains('\\') || s == "." || s == ".." {
        bail!("invalid identifier: {s:?}");
    }
    Ok(())
}
