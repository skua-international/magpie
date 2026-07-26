//! Blob storage for local (zip-uploaded) mods and missions, on a shared
//! volume mounted into the controller and, read-only, into every launcher
//! Pod (see `reconcile.rs`). Deliberately outside sync-daemon/steam-sync
//! entirely -- neither of these content types has anything to do with
//! Steam, so they don't belong in the reflink-claim/manifest-tracking
//! machinery that exists specifically to handle Steam depot updates.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
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
        std::fs::remove_dir_all(&dest)
            .context("failed to remove existing local mod directory before re-extracting")?;
    }
    std::fs::create_dir_all(&dest).context("failed to create local mod directory")?;

    let reader = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(reader).context("failed to read zip archive")?;
    archive
        .extract(&dest)
        .context("failed to extract zip archive")?;

    let mod_root = if dest.join("mod.cpp").is_file() {
        dest.clone()
    } else {
        // Look for a single top-level directory wrapping the actual mod
        // content (e.g. the zip was exported as "MyMod-main/mod.cpp").
        let entries: Vec<_> = std::fs::read_dir(&dest)
            .context("failed to list extracted mod directory")?
            .filter_map(|e| e.ok())
            .collect();
        match entries.as_slice() {
            [entry] if entry.path().is_dir() && entry.path().join("mod.cpp").is_file() => {
                entry.path()
            }
            _ => {
                bail!("extracted archive has no mod.cpp at its root or in a single wrapping folder")
            }
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

/// Total bytes of every regular file under a local mod's own directory --
/// local mods have no separate size bookkeeping (unlike Steam-backed
/// mods, tracked in sync-daemon's SyncState at sync time), so this is a
/// live directory walk on each call.
pub fn local_mod_size(root: &Path, unique_id: &str) -> u64 {
    let dir = local_mods_dir(root).join(unique_id);
    walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub fn delete_local_mod(root: &Path, unique_id: &str) -> Result<()> {
    validate_path_component(unique_id)?;
    let dest = local_mods_dir(root).join(unique_id);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).context("failed to delete local mod directory")?;
    }
    Ok(())
}

/// Sanitizes a mission's original filename for on-disk use -- doesn't
/// touch the UUID (that's the directory, see `write_mission`), just the
/// leaf name itself.
fn sanitize_mission_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Stored as `<uuid>/<sanitized name>` -- confirmed live: Arma does walk
/// mpmissions/ subdirectories looking for mission pbos, so nesting one
/// level per mission is enough to keep the UUID (stable and
/// collision-free across re-uploads, since a mission can be overwritten
/// with a different original filename) out of the leaf filename
/// entirely. That matters because Arma's mission browser and
/// mission-selection config (missionWhitelist[] etc.) key off exactly
/// that leaf filename's own `<name>.<world>` stem -- a `<uuid>__` prefix
/// baked into the filename itself (the previous scheme) became part of
/// that identity and broke whitelist matching outright.
fn mission_dir(root: &Path, id: Uuid) -> PathBuf {
    missions_dir(root).join(id.to_string())
}

pub fn write_mission(root: &Path, id: Uuid, name: &str, pbo_bytes: &[u8]) -> Result<PathBuf> {
    let dir = mission_dir(root, id);

    // A re-upload (overwrite) may change the filename -- drop this UUID's
    // whole directory before writing the new one, same idempotent
    // "start clean" approach as before, just scoped to a directory now
    // instead of a filename prefix.
    delete_mission_file(root, id)?;
    std::fs::create_dir_all(&dir).context("failed to create mission directory")?;

    let path = dir.join(sanitize_mission_name(name));
    std::fs::write(&path, pbo_bytes).context("failed to write mission file")?;
    Ok(path)
}

pub fn delete_mission_file(root: &Path, id: Uuid) -> Result<()> {
    let dir = mission_dir(root, id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("failed to delete mission directory"),
    }
}

/// Guards against a `unique_id` containing path traversal (`..`, `/`) --
/// it's caller-assigned and used directly as a directory name.
fn validate_path_component(s: &str) -> Result<()> {
    if s.is_empty() || s.contains('/') || s.contains('\\') || s == "." || s == ".." {
        bail!("invalid identifier: {s:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_mission_leaf_filename_has_no_uuid_prefix() {
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::now_v7();
        let path = write_mission(root.path(), id, "skua_training.Malden.pbo", b"pbo bytes").unwrap();

        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "skua_training.Malden.pbo",
            "Arma keys mission identity off exactly this leaf filename's <name>.<world> stem -- \
             a uuid baked into it (the previous scheme) broke missionWhitelist matching"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"pbo bytes");
    }

    #[test]
    fn delete_mission_file_removes_the_whole_directory() {
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::now_v7();
        let path = write_mission(root.path(), id, "outpost.Altis.pbo", b"data").unwrap();
        assert!(path.exists());

        delete_mission_file(root.path(), id).unwrap();

        assert!(!path.exists());
        assert!(!mission_dir(root.path(), id).exists());
    }

    #[test]
    fn delete_mission_file_on_nonexistent_id_is_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        delete_mission_file(root.path(), Uuid::now_v7()).unwrap();
    }

    #[test]
    fn reupload_with_different_name_replaces_the_old_file() {
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::now_v7();
        write_mission(root.path(), id, "old_name.Altis.pbo", b"v1").unwrap();

        let path = write_mission(root.path(), id, "new_name.Altis.pbo", b"v2").unwrap();

        assert!(!mission_dir(root.path(), id).join("old_name.Altis.pbo").exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"v2");
    }
}
