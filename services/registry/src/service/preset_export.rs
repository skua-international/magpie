//! `ModSourceService.ExportPreset`'s logic: which mods a set of sources
//! contributes, and in what order they're written out. Split from
//! mod_source.rs so it can be tested without a cluster or sync-daemon.

use std::collections::{HashMap, HashSet};

use crd::{ModSource, ModSourceInput};
use kube::ResourceExt;

/// What exporting `requested` would contain, before titles are looked up.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExportPlan {
    /// Distinct mod ids, in first-appearance order across the requested
    /// sources.
    pub mod_ids: Vec<u64>,
    pub skipped_local: Vec<String>,
    pub missing: Vec<String>,
    pub unresolved: Vec<String>,
}

/// Combine `requested` sources' resolved mods.
///
/// Reads `status.resolved_mod_ids`, the list sync-daemon's reconciler
/// last wrote -- the same list a server built from these sources loads --
/// rather than resolving anything afresh, so the preset matches what the
/// cluster actually runs rather than what Steam says now.
pub fn plan(requested: &[String], sources: &[ModSource]) -> ExportPlan {
    let by_name: HashMap<String, &ModSource> = sources.iter().map(|s| (s.name_any(), s)).collect();
    let mut out = ExportPlan::default();
    let mut seen_mods = HashSet::new();
    let mut seen_sources = HashSet::new();

    for id in requested {
        if !seen_sources.insert(id.as_str()) {
            continue;
        }
        let Some(source) = by_name.get(id) else {
            out.missing.push(id.clone());
            continue;
        };
        if matches!(source.spec.source, ModSourceInput::Local { .. }) {
            out.skipped_local.push(id.clone());
            continue;
        }
        let resolved = source
            .status
            .as_ref()
            .map(|s| s.resolved_mod_ids.as_slice())
            .unwrap_or_default();
        if resolved.is_empty() {
            out.unresolved.push(id.clone());
            continue;
        }
        out.mod_ids
            .extend(resolved.iter().copied().filter(|m| seen_mods.insert(*m)));
    }
    out
}

/// Titles for `mod_ids`, from `known` where it has a non-empty one, and
/// an id-based placeholder otherwise -- a preset row needs *some* display
/// name, and the Launcher re-reads the real one from Steam on import
/// anyway.
pub fn titled(mod_ids: &[u64], known: &HashMap<u64, String>) -> Vec<(u64, String)> {
    mod_ids
        .iter()
        .map(|id| {
            let title = known
                .get(id)
                .filter(|t| !t.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Workshop item {id}"));
            (*id, title)
        })
        .collect()
}

/// Ids in `mod_ids` with no usable title in `known` -- the only ones worth
/// asking Steam about.
pub fn untitled(mod_ids: &[u64], known: &HashMap<u64, String>) -> Vec<u64> {
    mod_ids
        .iter()
        .copied()
        .filter(|id| known.get(id).is_none_or(|t| t.trim().is_empty()))
        .collect()
}

/// Alphabetical by title, ignoring case, the way the Launcher's own
/// exports are ordered -- a player comparing this against their own list
/// finds things where they expect them. Ties fall back to id so the
/// output is stable.
pub fn launcher_order(mut mods: Vec<(u64, String)>) -> Vec<(u64, String)> {
    mods.sort_by(|(a_id, a), (b_id, b)| {
        a.to_lowercase().cmp(&b.to_lowercase()).then(a_id.cmp(b_id))
    });
    mods
}

/// A filename-and-preset-name-safe default when the caller gave no name.
pub fn preset_name(requested: &str) -> String {
    let name = requested.trim();
    if name.is_empty() {
        "magpie".to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use crd::{ModSourceSpec, ModSourceStatus};

    use super::*;

    fn steam(name: &str, resolved: &[u64]) -> ModSource {
        let mut s = ModSource::new(
            name,
            ModSourceSpec {
                source: ModSourceInput::SteamUrl(format!(
                    "https://steamcommunity.com/sharedfiles/filedetails/?id={name}"
                )),
            },
        );
        s.status = Some(ModSourceStatus {
            resolved_mod_ids: resolved.to_vec(),
            ..Default::default()
        });
        s
    }

    fn local(name: &str) -> ModSource {
        ModSource::new(
            name,
            ModSourceSpec {
                source: ModSourceInput::Local {
                    unique_id: name.to_string(),
                },
            },
        )
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn combines_sources_in_order_without_duplicates() {
        // A server's sources often overlap -- CBA in every one of them.
        let sources = [steam("a", &[1, 2, 3]), steam("b", &[3, 4, 1])];
        let plan = plan(&ids(&["a", "b"]), &sources);
        assert_eq!(plan.mod_ids, vec![1, 2, 3, 4]);
        assert!(plan.missing.is_empty() && plan.skipped_local.is_empty());
    }

    #[test]
    fn one_source_exports_alone() {
        let sources = [steam("a", &[1, 2]), steam("b", &[3])];
        assert_eq!(plan(&ids(&["b"]), &sources).mod_ids, vec![3]);
    }

    #[test]
    fn deleted_sources_are_reported_not_fatal() {
        // A server's spec can still name a source deleted since.
        let plan = plan(&ids(&["gone", "a"]), &[steam("a", &[1])]);
        assert_eq!(plan.mod_ids, vec![1]);
        assert_eq!(plan.missing, vec!["gone"]);
    }

    #[test]
    fn local_sources_are_left_out_and_named() {
        let plan = plan(
            &ids(&["skua_custom", "a"]),
            &[local("skua_custom"), steam("a", &[1])],
        );
        assert_eq!(plan.mod_ids, vec![1]);
        assert_eq!(plan.skipped_local, vec!["skua_custom"]);
    }

    #[test]
    fn unresolved_sources_are_named() {
        let mut pending = steam("pending", &[]);
        pending.status = None;
        let plan = plan(&ids(&["pending", "empty"]), &[pending, steam("empty", &[])]);
        assert!(plan.mod_ids.is_empty());
        assert_eq!(plan.unresolved, vec!["pending", "empty"]);
    }

    #[test]
    fn a_source_listed_twice_counts_once() {
        let plan = plan(&ids(&["gone", "gone", "a", "a"]), &[steam("a", &[1])]);
        assert_eq!(plan.missing, vec!["gone"]);
        assert_eq!(plan.mod_ids, vec![1]);
    }

    #[test]
    fn titles_fall_back_to_the_id() {
        let known = HashMap::from([(1, "CBA_A3".to_string()), (2, "  ".to_string())]);
        assert_eq!(
            titled(&[1, 2, 3], &known),
            vec![
                (1, "CBA_A3".to_string()),
                (2, "Workshop item 2".to_string()),
                (3, "Workshop item 3".to_string()),
            ]
        );
        assert_eq!(untitled(&[1, 2, 3], &known), vec![2, 3]);
    }

    #[test]
    fn launcher_order_is_case_insensitive_and_stable() {
        let ordered = launcher_order(vec![
            (3, "cba_a3".to_string()),
            (1, "ACE".to_string()),
            (2, "CBA_A3".to_string()),
            (4, "3den Enhanced".to_string()),
        ]);
        assert_eq!(
            ordered.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![4, 1, 2, 3]
        );
    }

    #[test]
    fn blank_names_get_a_default() {
        assert_eq!(preset_name("  "), "magpie");
        assert_eq!(preset_name(" Thursday Ops "), "Thursday Ops");
    }
}
