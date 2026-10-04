//! The registry half of `AdminService`'s Workshop collection RPCs:
//! preset parsing, and converting between registry's protos and
//! sync-daemon's, which does the Steam work.
//!
//! Kept out of admin.rs so it can be tested as plain functions -- the
//! handlers there are just "convert, call sync-daemon, convert back".

use buffa::EnumValue;
use buffa::enumeration::Enumeration;
use connectrpc::{ConnectError, ErrorCode};

use protocol::proto::registry::v1::{
    CollectionVisibility as RegistryVisibility, PublishWorkshopCollectionResponse,
    ResolveWorkshopItemsResponse, WorkshopCollection, WorkshopCollectionSummary, WorkshopItem,
};
use protocol::proto::sync::v1::{
    CollectionSummary as SyncSummary, CollectionVisibility as SyncVisibility,
    GetCollectionResponse, PublishCollectionResponse, WorkshopItem as SyncItem,
};
use workshop_parse::workshop_url as collection_url;

/// Every distinct Workshop ID `preset_html` and then `ids` name, in that
/// order -- what ResolveWorkshopItems is asked to resolve.
///
/// De-duplicated here rather than leaving it to the Steam side:
/// `workshop_parse::parse_preset_html` returns one entry per
/// `filedetails/?id=` link, and a Launcher export links every mod twice
/// (the href and the link text), so the raw parse is not a list of mods.
pub fn candidate_ids(preset_html: &str, ids: &[u64]) -> Result<Vec<u64>, ConnectError> {
    let mut seen = std::collections::HashSet::new();
    let candidates: Vec<u64> = workshop_parse::parse_preset_html(preset_html)
        .into_iter()
        .chain(ids.iter().copied())
        .filter(|id| *id != 0 && seen.insert(*id))
        .collect();

    if candidates.is_empty() {
        // An upload with no links in it is almost always the wrong file,
        // so say what was expected rather than "nothing to resolve".
        let msg = if preset_html.trim().is_empty() {
            "nothing to resolve -- give a preset export or at least one Workshop id"
        } else {
            "no Steam Workshop links found -- expected an Arma 3 Launcher preset export \
             (Launcher > MODS > PRESET > EXPORT)"
        };
        return Err(ConnectError::invalid_argument(msg));
    }
    Ok(candidates)
}

/// registry's `CollectionVisibility` to sync's identically-numbered one.
///
/// Converted explicitly, arm by arm, rather than by passing the integer
/// across: the two enums are declared in separate protos that nothing
/// forces to stay in step, and a silent pass-through would turn a future
/// divergence into a wrong visibility rather than a compile error.
/// UNSPECIFIED is refused here, so an unset field never reaches Steam.
pub fn to_sync_visibility(value: i32) -> Result<SyncVisibility, ConnectError> {
    match RegistryVisibility::from_i32(value) {
        Some(RegistryVisibility::COLLECTION_VISIBILITY_PUBLIC) => {
            Ok(SyncVisibility::COLLECTION_VISIBILITY_PUBLIC)
        }
        Some(RegistryVisibility::COLLECTION_VISIBILITY_FRIENDS_ONLY) => {
            Ok(SyncVisibility::COLLECTION_VISIBILITY_FRIENDS_ONLY)
        }
        Some(RegistryVisibility::COLLECTION_VISIBILITY_PRIVATE) => {
            Ok(SyncVisibility::COLLECTION_VISIBILITY_PRIVATE)
        }
        Some(RegistryVisibility::COLLECTION_VISIBILITY_UNLISTED) => {
            Ok(SyncVisibility::COLLECTION_VISIBILITY_UNLISTED)
        }
        Some(RegistryVisibility::COLLECTION_VISIBILITY_UNSPECIFIED) | None => {
            Err(ConnectError::invalid_argument(
                "visibility must be set explicitly (public, friends-only, private, or unlisted)",
            ))
        }
    }
}

/// The reverse of [`to_sync_visibility`]. Anything unknown comes back as
/// UNSPECIFIED, which is also what sync-daemon sends for a Steam value it
/// doesn't recognise.
pub fn to_registry_visibility(value: EnumValue<SyncVisibility>) -> EnumValue<RegistryVisibility> {
    match SyncVisibility::from_i32(value.to_i32()) {
        Some(SyncVisibility::COLLECTION_VISIBILITY_PUBLIC) => {
            RegistryVisibility::COLLECTION_VISIBILITY_PUBLIC
        }
        Some(SyncVisibility::COLLECTION_VISIBILITY_FRIENDS_ONLY) => {
            RegistryVisibility::COLLECTION_VISIBILITY_FRIENDS_ONLY
        }
        Some(SyncVisibility::COLLECTION_VISIBILITY_PRIVATE) => {
            RegistryVisibility::COLLECTION_VISIBILITY_PRIVATE
        }
        Some(SyncVisibility::COLLECTION_VISIBILITY_UNLISTED) => {
            RegistryVisibility::COLLECTION_VISIBILITY_UNLISTED
        }
        Some(SyncVisibility::COLLECTION_VISIBILITY_UNSPECIFIED) | None => {
            RegistryVisibility::COLLECTION_VISIBILITY_UNSPECIFIED
        }
    }
    .into()
}

/// Relay a sync-daemon error to registry's own caller.
///
/// Its message is what's worth showing (it names the mod or the reason),
/// so that is kept. Its code is kept only when it describes the caller's
/// request: an `unauthenticated` from sync-daemon would otherwise reach
/// the web UI's auth interceptor and be taken for the *user's* token
/// having expired, signing them out over a Steam-side problem.
pub fn relay(err: ConnectError) -> ConnectError {
    let code = match err.code {
        ErrorCode::InvalidArgument | ErrorCode::NotFound | ErrorCode::FailedPrecondition => {
            err.code
        }
        _ => ErrorCode::Internal,
    };
    let message = err
        .message
        .unwrap_or_else(|| "sync-daemon returned an error with no message".to_string());
    ConnectError::new(code, message)
}

pub fn items(items: Vec<SyncItem>) -> Vec<WorkshopItem> {
    items
        .into_iter()
        .map(|m| WorkshopItem {
            id: m.id,
            title: m.title,
            file_size: m.file_size,
            ..Default::default()
        })
        .collect()
}

pub fn resolution(
    resp: protocol::proto::sync::v1::ResolveWorkshopItemsResponse,
) -> ResolveWorkshopItemsResponse {
    ResolveWorkshopItemsResponse {
        mods: items(resp.mods),
        unresolved: resp.unresolved,
        ..Default::default()
    }
}

pub fn summary(c: SyncSummary) -> WorkshopCollectionSummary {
    WorkshopCollectionSummary {
        id: c.id,
        title: c.title,
        visibility: to_registry_visibility(c.visibility),
        item_count: c.item_count,
        updated_at_unix_ms: c.updated_at_unix_ms,
        url: collection_url(c.id),
        ..Default::default()
    }
}

pub fn collection(c: GetCollectionResponse) -> WorkshopCollection {
    WorkshopCollection {
        id: c.id,
        title: c.title,
        description: c.description,
        visibility: to_registry_visibility(c.visibility),
        url: collection_url(c.id),
        owned: c.owned,
        updated_at_unix_ms: c.updated_at_unix_ms,
        mods: items(c.mods),
        unresolved: c.unresolved,
        ..Default::default()
    }
}

pub fn published(p: PublishCollectionResponse) -> PublishWorkshopCollectionResponse {
    PublishWorkshopCollectionResponse {
        collection_id: p.collection_id,
        url: p.url,
        mods: items(p.mods),
        unresolved: p.unresolved,
        created: p.created,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real export this feature was built against: 115 mods, every
    /// one linked twice, a few over plain http.
    const BEARZ_SAHATRA: &str = include_str!("testdata/BearzSahatra.html");

    const PRESET: &str = r#"
        <tr data-type="ModContainer">
          <td data-type="DisplayName">CBA_A3</td>
          <td><a href="https://steamcommunity.com/sharedfiles/filedetails/?id=450814997">x</a></td>
        </tr>
        <tr data-type="ModContainer">
          <td data-type="DisplayName">3den Enhanced</td>
          <td><a href="https://steamcommunity.com/sharedfiles/filedetails/?id=623475643">x</a></td>
        </tr>
    "#;

    #[test]
    fn real_export_yields_one_candidate_per_mod_in_order() {
        let ids = candidate_ids(BEARZ_SAHATRA, &[]).unwrap();
        assert_eq!(ids.len(), 115);
        // First and last rows of the export, and an http:// one.
        assert_eq!(ids[0], 2983059191); // @SMA Optics
        assert_eq!(*ids.last().unwrap(), 2450921295); // Zulu Headless Client
        assert!(ids.contains(&766491311)); // http://steamcommunity.com/...
    }

    #[test]
    fn extracts_preset_ids_in_order() {
        assert_eq!(
            candidate_ids(PRESET, &[]).unwrap(),
            vec![450814997, 623475643]
        );
    }

    #[test]
    fn collapses_repeated_ids() {
        let doubled = format!("{PRESET}{PRESET}");
        assert_eq!(
            candidate_ids(&doubled, &[]).unwrap(),
            vec![450814997, 623475643]
        );
    }

    #[test]
    fn explicit_ids_follow_the_preset_and_dedupe_against_it() {
        assert_eq!(
            candidate_ids(PRESET, &[1779063631, 450814997]).unwrap(),
            vec![450814997, 623475643, 1779063631]
        );
    }

    #[test]
    fn ids_alone_are_enough() {
        assert_eq!(candidate_ids("", &[5, 6, 5]).unwrap(), vec![5, 6]);
    }

    #[test]
    fn zero_ids_are_dropped() {
        // proto3's absent uint64; never a real Workshop id.
        assert_eq!(candidate_ids("", &[0, 7]).unwrap(), vec![7]);
        assert!(candidate_ids("", &[0]).is_err());
    }

    #[test]
    fn rejects_html_with_no_workshop_links_naming_the_expected_file() {
        let err = candidate_ids("<html><body>not a preset</body></html>", &[]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(
            err.message
                .as_deref()
                .unwrap_or("")
                .contains("preset export"),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_empty_input() {
        let err = candidate_ids("", &[]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(
            err.message
                .as_deref()
                .unwrap_or("")
                .contains("nothing to resolve")
        );
    }

    #[test]
    fn visibility_converts_arm_by_arm_both_ways() {
        for (registry, sync) in [
            (
                RegistryVisibility::COLLECTION_VISIBILITY_PUBLIC,
                SyncVisibility::COLLECTION_VISIBILITY_PUBLIC,
            ),
            (
                RegistryVisibility::COLLECTION_VISIBILITY_FRIENDS_ONLY,
                SyncVisibility::COLLECTION_VISIBILITY_FRIENDS_ONLY,
            ),
            (
                RegistryVisibility::COLLECTION_VISIBILITY_PRIVATE,
                SyncVisibility::COLLECTION_VISIBILITY_PRIVATE,
            ),
            (
                RegistryVisibility::COLLECTION_VISIBILITY_UNLISTED,
                SyncVisibility::COLLECTION_VISIBILITY_UNLISTED,
            ),
        ] {
            assert_eq!(to_sync_visibility(registry.to_i32()).unwrap(), sync);
            assert_eq!(
                to_registry_visibility(sync.into()).to_i32(),
                registry.to_i32()
            );
        }
    }

    /// The safety property: an unset field, or a value from a newer
    /// client this build doesn't know, must never resolve to a
    /// visibility -- least of all Public.
    #[test]
    fn visibility_refuses_unspecified_and_unknown() {
        assert!(to_sync_visibility(0).is_err());
        assert!(to_sync_visibility(99).is_err());
        assert!(to_sync_visibility(-1).is_err());
    }

    #[test]
    fn unknown_visibility_from_sync_reads_as_unspecified() {
        assert_eq!(to_registry_visibility(EnumValue::from(99)).to_i32(), 0);
    }

    #[test]
    fn relay_keeps_request_errors_and_their_message() {
        let err = relay(ConnectError::invalid_argument(
            "candidate_ids must not be empty",
        ));
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(
            err.message.as_deref(),
            Some("candidate_ids must not be empty")
        );
    }

    #[test]
    fn relay_never_passes_on_an_auth_error() {
        // Would read to the web UI as the user's own session expiring.
        for e in [
            ConnectError::new(ErrorCode::Unauthenticated, "steam"),
            ConnectError::permission_denied("steam"),
        ] {
            let relayed = relay(e);
            assert_eq!(relayed.code, ErrorCode::Internal);
            assert_eq!(relayed.message.as_deref(), Some("steam"));
        }
    }

    #[test]
    fn collection_gets_its_url_built_here() {
        let c = collection(GetCollectionResponse {
            id: 3792213005,
            title: "Sahatra".into(),
            owned: true,
            mods: vec![SyncItem {
                id: 450814997,
                title: "CBA_A3".into(),
                file_size: 1,
                ..Default::default()
            }],
            unresolved: vec![9],
            visibility: SyncVisibility::COLLECTION_VISIBILITY_UNLISTED.into(),
            ..Default::default()
        });
        assert_eq!(
            c.url,
            "https://steamcommunity.com/sharedfiles/filedetails/?id=3792213005"
        );
        assert!(c.owned);
        assert_eq!(c.mods[0].title, "CBA_A3");
        assert_eq!(c.unresolved, vec![9]);
        assert_eq!(
            c.visibility.to_i32(),
            RegistryVisibility::COLLECTION_VISIBILITY_UNLISTED.to_i32()
        );
    }

    #[test]
    fn summary_gets_its_url_built_here() {
        let s = summary(SyncSummary {
            id: 7,
            item_count: 115,
            ..Default::default()
        });
        assert_eq!(s.url, collection_url(7));
        assert_eq!(s.item_count, 115);
    }
}
