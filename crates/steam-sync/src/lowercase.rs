//! Repair for workshop trees folded by an earlier version of this code.
//!
//! Arma's engine builds internal paths in lowercase and, on a
//! case-sensitive filesystem, refuses anything that doesn't match:
//!
//! ```text
//! Warning Message: The filename '/arma3/content/workshop/2132195038\addons\AIMEE_main.pbo'
//!                  is not lowercase. You have to convert it!
//! ```
//!
//! Folding now happens where the path is derived from the depot manifest
//! (`steamdepot`'s `PathOptions::lowercase`), so freshly synced content is
//! already lowercase and stays that way across updates.
//!
//! What that doesn't fix is trees synced *before* it. Those went through a
//! rename-after-download pass, which quietly corrupted them: the manifest
//! is what the next sync verifies against, and `prepare_directory_tree`
//! only ever creates, never prunes, so a later content update re-created
//! every renamed file under its manifest casing and left the folded copies
//! beside them. Observed live on `2132195038`, whose `addons/` held both
//! `AIMEE_main.pbo` and `aimee_main.pbo` for every PBO in the mod -- Arma
//! loads both and reports duplicate classes.
//!
//! Such a tree is marked synced, so nothing re-downloads it and the
//! manifest-side fix never reaches it. [`prune_mixed_case_duplicates`]
//! removes the stranded mixed-case copies and [`lowercase_tree`] folds
//! whatever is left, which converges any tree on the same state the new
//! download path produces. Both are idempotent and cost one directory walk
//! once a tree is healthy.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};
use tracing::warn;

/// What a repair pass actually did -- logged by the caller, and what a
/// test asserts on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FoldStats {
    /// Entries renamed to their lowercase form.
    pub renamed: usize,
    /// Entries left alone because folding them would have collided with a
    /// different existing entry. Never silent -- each is warned about,
    /// since it means content Arma cannot fully load.
    pub collisions: usize,
}

/// Fold every file and directory under `root` to a lowercase name.
///
/// Depth-first: a directory's contents are folded before the directory
/// itself, so a parent rename never invalidates a path still being walked.
/// `root` itself is untouched -- it's the published-file-id directory,
/// which is digits anyway.
pub fn lowercase_tree(root: &Path) -> Result<FoldStats> {
    let mut stats = FoldStats::default();
    if !root.is_dir() {
        return Ok(stats);
    }
    fold_dir(root, &mut stats)?;
    Ok(stats)
}

fn fold_dir(dir: &Path, stats: &mut FoldStats) -> Result<()> {
    for entry in read_dir_sorted(dir)? {
        let path = entry.path();
        // DirEntry::file_type does not follow symlinks, which is what we
        // want: a symlink gets its own name folded and is never descended
        // into (one pointing up the tree would otherwise recurse forever).
        let is_dir = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", path.display()))?
            .is_dir();
        if is_dir {
            fold_dir(&path, stats)?;
        }

        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            // A non-UTF-8 name has no meaningful lowercase form, and Arma
            // could not address it anyway.
            warn!("skipping non-UTF-8 filename in {}", dir.display());
            continue;
        };
        let lower = name.to_lowercase();
        if lower == name {
            continue;
        }

        let target = dir.join(&lower);
        // `Some(true)` is a case-insensitive filesystem reporting the
        // target as existing because it *is* the source; renaming is
        // still how the name changes there, so only `Some(false)` -- a
        // genuinely different object -- blocks the fold.
        if same_entry(&path, &target)? == Some(false) {
            // Reachable only for content this repair didn't cover -- a
            // depot genuinely shipping two names differing by case.
            // steamdepot now refuses to fold such a depot at all, so the
            // right move here is to leave both and say so rather than
            // pick a winner.
            warn!(
                "not folding {}: {} exists and is a different file",
                path.display(),
                target.display()
            );
            stats.collisions += 1;
            continue;
        }

        fs::rename(&path, &target).with_context(|| {
            format!(
                "failed to rename {} -> {}",
                path.display(),
                target.display()
            )
        })?;
        stats.renamed += 1;
    }
    Ok(())
}

/// Remove mixed-case entries stranded beside their lowercase twin.
///
/// Needs no manifest: within a workshop tree, an entry whose name is not
/// already lowercase and whose lowercase sibling exists as a *different*
/// object is exactly the leftover the old rename-after-download pass
/// created. The lowercase copy is the one the new download path maintains,
/// so the mixed-case one is what goes.
///
/// A directory is removed only once it is empty, which it will be since
/// its contents are pruned first. That matters: leaving an emptied
/// `Addons/` behind would make the fold of anything else collide with it.
/// `remove_dir` refuses a non-empty directory, so nothing unaccounted-for
/// can be taken out with it.
///
/// An entry with no lowercase twin is left for [`lowercase_tree`] to
/// rename -- pruning it would delete the mod's only copy.
pub fn prune_mixed_case_duplicates(root: &Path) -> Result<usize> {
    let mut pruned = 0;
    if root.is_dir() {
        prune_dir(root, &mut pruned)?;
    }
    Ok(pruned)
}

fn prune_dir(dir: &Path, pruned: &mut usize) -> Result<()> {
    for entry in read_dir_sorted(dir)? {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", path.display()))?;
        // Depth-first, so a directory is only considered once everything
        // inside it has been.
        if file_type.is_dir() {
            prune_dir(&path, pruned)?;
        }

        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let lower = name.to_lowercase();
        if lower == name {
            continue;
        }
        let twin = dir.join(&lower);
        // No twin means this is the only copy -- folding is the fix, not
        // deletion.
        if fs::symlink_metadata(&twin).is_err() {
            continue;
        }
        // Guards a case-insensitive filesystem, where the two paths are
        // one object and "removing the duplicate" deletes the content.
        if matches!(same_entry(&path, &twin)?, Some(true)) {
            continue;
        }

        if file_type.is_dir() {
            // The duplication can sit at the directory level with
            // perfectly ordinary names inside it -- `Addons/keep.pbo`
            // beside `addons/keep.pbo` duplicates a file whose own name
            // needs no folding, so the rule above never sees it. Drop
            // whatever the twin already accounts for, then remove the
            // directory if that emptied it.
            prune_into_twin(&path, &twin, pruned)?;
            match fs::remove_dir(&path) {
                Ok(()) => {
                    warn!(
                        "removed emptied mixed-case directory {} (kept {})",
                        path.display(),
                        twin.display()
                    );
                    *pruned += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // Something in it isn't accounted for by the lowercase
                // twin; leave it and let the fold's collision warning
                // report it rather than guessing.
                Err(e) => warn!(
                    "leaving {} in place, not empty or not removable: {e}",
                    path.display()
                ),
            }
        } else {
            warn!(
                "removing stranded mixed-case duplicate {} (kept {})",
                path.display(),
                twin.display()
            );
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            *pruned += 1;
        }
    }
    Ok(())
}

/// Remove entries of `dir` that `twin` already holds under the folded
/// name, recursing so a nested pair collapses from the leaves inward.
///
/// Only ever deletes something with a counterpart in `twin`: an entry the
/// twin doesn't have is content that would be lost, and is left where it
/// is so the enclosing `remove_dir` fails and the whole pair survives to
/// be reported.
fn prune_into_twin(dir: &Path, twin: &Path, pruned: &mut usize) -> Result<()> {
    for entry in read_dir_sorted(dir)? {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_lowercase) else {
            continue;
        };
        let counterpart = twin.join(name);
        let Ok(counterpart_meta) = fs::symlink_metadata(&counterpart) else {
            continue;
        };
        if matches!(same_entry(&path, &counterpart)?, Some(true)) {
            continue;
        }

        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", path.display()))?;
        if file_type.is_dir() && counterpart_meta.is_dir() {
            prune_into_twin(&path, &counterpart, pruned)?;
            if fs::remove_dir(&path).is_ok() {
                *pruned += 1;
            }
        } else if file_type.is_dir() != counterpart_meta.is_dir() {
            // A file on one side and a directory on the other is not a
            // duplicate of anything -- refuse to guess.
            warn!(
                "leaving {} in place: {} is not the same kind of entry",
                path.display(),
                counterpart.display()
            );
        } else {
            warn!(
                "removing stranded mixed-case duplicate {} (kept {})",
                path.display(),
                counterpart.display()
            );
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            *pruned += 1;
        }
    }
    Ok(())
}

/// Directory entries in a stable order, so a pass over a tree behaves the
/// same way twice and a test can rely on what it sees.
fn read_dir_sorted(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to list {}", dir.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

/// `None` if `target` doesn't exist; `Some(true)` if it is the same
/// filesystem object as `source`, `Some(false)` if a different one.
///
/// Uses `symlink_metadata` rather than `Path::exists` so a broken symlink
/// still counts as occupying the name, and compares device+inode rather
/// than paths so a case-insensitive filesystem is not mistaken for a
/// collision.
fn same_entry(source: &Path, target: &Path) -> Result<Option<bool>> {
    let Ok(target_meta) = fs::symlink_metadata(target) else {
        return Ok(None);
    };
    let source_meta = fs::symlink_metadata(source)
        .with_context(|| format!("failed to stat {}", source.display()))?;
    Ok(Some(
        source_meta.dev() == target_meta.dev() && source_meta.ino() == target_meta.ino(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(root: &Path, relative: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, relative.as_bytes()).unwrap();
    }

    fn tree(root: &Path) -> Vec<String> {
        let mut found = Vec::new();
        fn walk(dir: &Path, prefix: &Path, out: &mut Vec<String>) {
            for e in read_dir_sorted(dir).unwrap() {
                let rel = prefix.join(e.file_name());
                if e.file_type().unwrap().is_dir() {
                    walk(&e.path(), &rel, out);
                } else {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
        walk(root, Path::new(""), &mut found);
        found
    }

    /// The whole repair, on the shape seen live: every PBO present under
    /// both spellings.
    #[test]
    fn repairs_the_tree_seen_live() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for name in ["AIMEE_main.pbo", "AIMEE_group.pbo"] {
            write(root, &format!("addons/{name}"));
            write(root, &format!("addons/{}", name.to_lowercase()));
        }

        let pruned = prune_mixed_case_duplicates(root).unwrap();
        lowercase_tree(root).unwrap();

        assert_eq!(pruned, 2);
        assert_eq!(
            tree(root),
            vec!["addons/aimee_group.pbo", "addons/aimee_main.pbo"]
        );
    }

    /// A mixed-case file with no lowercase twin is the mod's only copy --
    /// it must be renamed, never deleted.
    #[test]
    fn a_lone_mixed_case_file_is_folded_not_pruned() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "Addons/PHEN_DoorKick.pbo");

        let pruned = prune_mixed_case_duplicates(root).unwrap();
        lowercase_tree(root).unwrap();

        assert_eq!(pruned, 0);
        assert_eq!(tree(root), vec!["addons/phen_doorkick.pbo"]);
    }

    #[test]
    fn folds_files_and_directories() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "Addons/Data_F_JCA_IE.pbo");
        write(root, "Keys/SomeKey.bikey");

        let stats = lowercase_tree(root).unwrap();

        assert_eq!(
            tree(root),
            vec!["addons/data_f_jca_ie.pbo", "keys/somekey.bikey"]
        );
        // Two files plus two directories.
        assert_eq!(stats.renamed, 4);
        assert_eq!(stats.collisions, 0);
    }

    #[test]
    fn a_healthy_tree_costs_nothing_and_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "addons/cba_main.pbo");

        // The steady state on every sync tick once steamdepot folds at
        // download time.
        assert_eq!(prune_mixed_case_duplicates(root).unwrap(), 0);
        assert_eq!(lowercase_tree(root).unwrap(), FoldStats::default());
        assert_eq!(tree(root), vec!["addons/cba_main.pbo"]);
    }

    #[test]
    fn repair_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "addons/AIMEE_main.pbo");
        write(root, "addons/aimee_main.pbo");

        prune_mixed_case_duplicates(root).unwrap();
        lowercase_tree(root).unwrap();
        let after_first = tree(root);

        assert_eq!(prune_mixed_case_duplicates(root).unwrap(), 0);
        assert_eq!(lowercase_tree(root).unwrap(), FoldStats::default());
        assert_eq!(tree(root), after_first);
    }

    #[test]
    fn prunes_a_stranded_directory_once_it_is_empty() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "Addons/keep.pbo");
        write(root, "addons/keep.pbo");

        let pruned = prune_mixed_case_duplicates(root).unwrap();

        // The duplicate file, then the directory it emptied.
        assert_eq!(pruned, 2);
        assert_eq!(tree(root), vec!["addons/keep.pbo"]);
    }

    /// The guardrail on removing directories: anything the lowercase twin
    /// does not account for keeps its contents.
    #[test]
    fn a_non_empty_mixed_case_directory_survives() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "Addons/keep.pbo");
        write(root, "addons/keep.pbo");
        write(root, "Addons/only_here.txt");

        prune_mixed_case_duplicates(root).unwrap();

        assert!(root.join("Addons/only_here.txt").is_file());
        assert!(root.join("addons/keep.pbo").is_file());
    }

    #[test]
    fn prunes_nested_stranded_directories_from_the_leaves_inward() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "Addons/Sub/deep.pbo");
        write(root, "addons/sub/deep.pbo");

        prune_mixed_case_duplicates(root).unwrap();

        assert!(!root.join("Addons").exists());
        assert_eq!(tree(root), vec!["addons/sub/deep.pbo"]);
    }

    #[test]
    fn folding_refuses_to_overwrite_a_different_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "addons/foo.pbo");
        write(root, "addons/FOO.pbo");

        // Fold without pruning first: both must survive, since choosing
        // one would silently lose content.
        let stats = lowercase_tree(root).unwrap();

        assert_eq!(stats.collisions, 1);
        assert_eq!(stats.renamed, 0);
        assert_eq!(tree(root), vec!["addons/FOO.pbo", "addons/foo.pbo"]);
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        // A mod whose download failed before creating anything.
        let missing = dir.path().join("nope");
        assert_eq!(lowercase_tree(&missing).unwrap(), FoldStats::default());
        assert_eq!(prune_mixed_case_duplicates(&missing).unwrap(), 0);
    }

    #[test]
    fn folds_a_symlink_by_name_without_following_it() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "addons/real.pbo");
        std::os::unix::fs::symlink("real.pbo", root.join("addons/Link.pbo")).unwrap();

        let stats = lowercase_tree(root).unwrap();

        let link = root.join("addons/link.pbo");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(stats.renamed, 1);
    }
}
