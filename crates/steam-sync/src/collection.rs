//! Publish, read, edit and delete Steam Workshop collections as the
//! cluster's own Steam account.
//!
//! Rides the same authenticated CM session as everything else here
//! (`PublishedFile.*` unified methods, exactly like
//! [`crate::steam::resolve_source_ids`]'s `GetDetails` call) rather than
//! driving `steamcommunity.com`'s web endpoints. That matters: the web
//! flow needs browser session cookies and an `addchild`/
//! `ajaxaddtocollections` pair whose parameter names differ from each
//! other and are unversioned, whereas `PublishedFile.Publish` and
//! `PublishedFile.SetCollectionChildren` are the same Steamworks surface
//! `ISteamUGC` exposes to shipped games and are already reachable with
//! the credentials sync-daemon holds.
//!
//! Nothing here is on the sync path -- creating a collection publishes
//! content *to* Steam and never touches the golden content tree.

use anyhow::{Context, Result, bail};
use steamdepot::connection::CmConnection;
use steamdepot::proto::PublishedFileDetails;
use tracing::info;

use crate::steam::{DEFAULT_RETRY_ATTEMPTS, with_retry, with_timeout};

/// Arma 3. Both the `appid` a collection is published under and the
/// `consumer_appid` it shows up in the Workshop of.
pub const ARMA3_APPID: u32 = 107410;

/// `EWorkshopFileType::k_EWorkshopFileTypeCollection`. Same constant
/// [`crate::steam`] matches on when deciding whether to expand a
/// candidate's children; repeated here so this module reads standalone.
const FILE_TYPE_COLLECTION: u32 = 2;

/// `EPublishedFileInfoMatchingFileType::k_PFI_MatchingFileType_Collections`
/// -- the *query* filter `GetUserFiles` takes, a different enum from the
/// `EWorkshopFileType` each result carries. Results are still checked
/// against [`FILE_TYPE_COLLECTION`] afterwards, so a wrong filter here
/// shows up as an empty list rather than as mods listed as collections.
const MATCHING_FILE_TYPE_COLLECTIONS: u32 = 1;

/// `GetUserFiles`' page-size ceiling.
const USER_FILES_PAGE_SIZE: u32 = 100;

/// Steam caps a collection's membership. Well past anything an Arma
/// preset realistically holds, but `SetCollectionChildren` replaces the
/// whole list in one call, so an over-long list would fail as a single
/// opaque rejection rather than partially -- worth naming the limit and
/// failing with a message that says which it was.
pub const MAX_CHILDREN: usize = 1000;

/// `ERemoteStoragePublishedFileVisibility`.
///
/// Unlisted is [`Visibility::default`] deliberately: the flow this exists
/// for hands someone a link, and a mistaken run should not put a
/// half-built collection into public Workshop search under the cluster
/// account's name. Making it public is a deliberate choice at the call
/// site, not what you get by leaving a field unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    Public,
    FriendsOnly,
    Private,
    #[default]
    Unlisted,
}

impl Visibility {
    /// The wire value Steam expects.
    pub fn as_steam(self) -> u32 {
        match self {
            Visibility::Public => 0,
            Visibility::FriendsOnly => 1,
            Visibility::Private => 2,
            Visibility::Unlisted => 3,
        }
    }

    /// Steam's numbering back. `None` for anything else (Steam has added
    /// values over the years -- "developer only" among them) rather than
    /// guessing which known one it is closest to.
    pub fn from_steam(value: u32) -> Option<Self> {
        match value {
            0 => Some(Visibility::Public),
            1 => Some(Visibility::FriendsOnly),
            2 => Some(Visibility::Private),
            3 => Some(Visibility::Unlisted),
            _ => None,
        }
    }

    /// The protos' `CollectionVisibility` numbering: Steam's, shifted by
    /// one so the proto3 zero is UNSPECIFIED.
    pub fn as_proto(self) -> i32 {
        self.as_steam() as i32 + 1
    }

    /// Parse the proto numbering back. Anything unrecognised -- including
    /// the proto3 default 0, which means "unset" as much as it means
    /// anything -- is refused rather than guessed at, so an unset field
    /// can't silently publish publicly.
    pub fn from_proto(value: i32) -> Result<Self> {
        match value {
            1 => Ok(Visibility::Public),
            2 => Ok(Visibility::FriendsOnly),
            3 => Ok(Visibility::Private),
            4 => Ok(Visibility::Unlisted),
            other => bail!("unknown visibility {other}"),
        }
    }
}

/// What `GetDetails` says about one collection, before its members are
/// resolved into mods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionDetails {
    pub id: u64,
    pub title: String,
    pub description: String,
    /// `None` for a visibility value this build doesn't know.
    pub visibility: Option<Visibility>,
    /// SteamID64 of the publishing account.
    pub creator: u64,
    pub time_updated: u32,
    /// Member IDs in the collection's own order. Mods or collections --
    /// still to be resolved.
    pub children: Vec<u64>,
}

/// A collection as listed by [`list_owned_collections`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionSummary {
    pub id: u64,
    pub title: String,
    pub visibility: Option<Visibility>,
    pub item_count: u32,
    pub time_updated: u32,
}

/// The SteamID64 `conn` is logged in as, refusing an anonymous session.
///
/// Every write here is done *as* an account, and `CmPool` can hand out a
/// slot that degraded to anonymous while others stayed logged in (see
/// `login_or_degrade_to_anonymous`) -- checking the connection actually in
/// hand gives a straight answer instead of a far less obvious Steam-side
/// rejection.
pub fn account_steam_id(conn: &CmConnection) -> Result<u64> {
    match conn.session() {
        Some(session) if session.authenticated => Ok(session.steam_id),
        _ => bail!(
            "this needs an authenticated Steam session, but the connection is anonymous -- \
             re-run the Steam login (magpiectl admin refresh-steam-auth, or Cluster in the web UI)"
        ),
    }
}

/// One unified-message call, decoded. `retry` only for calls that are safe
/// to repeat: a timed-out `Publish` may still have happened on Steam's
/// side, and retrying it would publish a second collection.
async fn call<Req, Resp>(
    conn: &mut CmConnection,
    method: &'static str,
    req: &Req,
    retry: bool,
) -> Result<Resp>
where
    Req: prost::Message,
    Resp: prost::Message + Default,
{
    let body = prost::Message::encode_to_vec(req);
    let attempts = if retry { DEFAULT_RETRY_ATTEMPTS } else { 1 };
    let resp_bytes = with_retry(method, attempts, conn, |conn| {
        let body = body.clone();
        Box::pin(async move { with_timeout(method, conn.service_method_call(method, &body)).await })
    })
    .await?;
    Resp::decode(resp_bytes).with_context(|| format!("failed to decode {method} response"))
}

/// Publish a new, empty collection owned by the logged-in account, and
/// return its `publishedfileid`.
///
/// Empty because `Publish` has no children field at all -- membership is
/// a second call ([`set_collection_children`]). A collection published
/// here therefore exists, briefly, with no mods in it; callers should
/// delete it again if populating it fails, rather than leaving an empty
/// collection behind under the account's name.
pub async fn publish_collection(
    conn: &mut CmConnection,
    title: &str,
    description: &str,
    visibility: Visibility,
) -> Result<u64> {
    let title = validate_title(title)?;

    let req = steamdepot::proto::CPublishedFilePublishRequest {
        appid: Some(ARMA3_APPID),
        consumer_appid: Some(ARMA3_APPID),
        title: Some(title.to_string()),
        file_description: Some(description.to_string()),
        file_type: Some(FILE_TYPE_COLLECTION),
        visibility: Some(visibility.as_steam()),
        // Steam derives a collection's own listing from its members; a
        // collection has no uploaded payload of its own, so both stay
        // empty rather than being given a placeholder cloud file.
        cloudfilename: Some(String::new()),
        preview_cloudfilename: Some(String::new()),
        ..Default::default()
    };

    let resp: steamdepot::proto::CPublishedFilePublishResponse =
        call(conn, "PublishedFile.Publish#1", &req, false)
            .await
            .context("failed to publish collection")?;

    match resp.publishedfileid {
        Some(id) if id != 0 => {
            info!("published collection {id} ({title:?}, {visibility:?})");
            Ok(id)
        }
        // Steam answering OK with no ID is not something to paper over
        // with a 0 -- every later call would be against a nonexistent
        // collection.
        _ => bail!("Steam accepted the publish but returned no collection id"),
    }
}

/// Change an existing collection's title, description and visibility.
pub async fn update_collection(
    conn: &mut CmConnection,
    collection_id: u64,
    title: &str,
    description: &str,
    visibility: Visibility,
) -> Result<()> {
    let title = validate_title(title)?;
    let req = steamdepot::proto::CPublishedFileUpdateRequest {
        appid: Some(ARMA3_APPID),
        publishedfileid: Some(collection_id),
        title: Some(title.to_string()),
        file_description: Some(description.to_string()),
        visibility: Some(visibility.as_steam()),
        ..Default::default()
    };
    let _: steamdepot::proto::CPublishedFileUpdateResponse =
        call(conn, "PublishedFile.Update#1", &req, true)
            .await
            .with_context(|| format!("failed to update collection {collection_id}"))?;
    info!("updated collection {collection_id} ({title:?}, {visibility:?})");
    Ok(())
}

/// Replace `collection_id`'s membership with exactly `children`.
///
/// A set, not an append -- which is what makes re-running this against an
/// updated preset idempotent, with no diffing against current membership
/// and no way to end up with mods that left the preset still listed.
pub async fn set_collection_children(
    conn: &mut CmConnection,
    collection_id: u64,
    children: &[u64],
) -> Result<()> {
    let children = validate_children(children)?;

    let req = steamdepot::proto::CPublishedFileSetCollectionChildrenRequest {
        appid: Some(ARMA3_APPID),
        publishedfileid: Some(collection_id),
        children: children.clone(),
    };
    let _: steamdepot::proto::CPublishedFileSetCollectionChildrenResponse = call(
        conn,
        "PublishedFile.SetCollectionChildren#1",
        &req,
        true,
    )
    .await
    .with_context(|| format!("failed to set children on collection {collection_id}"))?;

    info!(
        "set {} members on collection {collection_id}",
        children.len()
    );
    Ok(())
}

/// Delete a published file. Only ever pointed at a collection by the
/// callers here, but Steam itself doesn't care which kind it is -- the
/// ownership and file-type checks belong to the caller.
pub async fn delete_collection(conn: &mut CmConnection, collection_id: u64) -> Result<()> {
    let req = steamdepot::proto::CPublishedFileDeleteRequest {
        appid: Some(ARMA3_APPID),
        publishedfileid: Some(collection_id),
    };
    let _: steamdepot::proto::CPublishedFileDeleteResponse =
        call(conn, "PublishedFile.Delete#1", &req, true)
            .await
            .with_context(|| format!("failed to delete collection {collection_id}"))?;
    info!("deleted collection {collection_id}");
    Ok(())
}

/// One collection's own details and member IDs.
///
/// Errors if `collection_id` isn't visible to this account or isn't a
/// collection at all -- a mod's `children` are its Required Items, and
/// treating those as membership would be a confusing thing to edit.
pub async fn get_collection_details(
    conn: &mut CmConnection,
    collection_id: u64,
) -> Result<CollectionDetails> {
    let req = steamdepot::proto::CPublishedFileGetDetailsRequest {
        publishedfileids: vec![collection_id],
        includechildren: Some(true),
        ..Default::default()
    };
    let resp: steamdepot::proto::CPublishedFileGetDetailsResponse =
        call(conn, "PublishedFile.GetDetails#1", &req, true)
            .await
            .with_context(|| format!("failed to look up collection {collection_id}"))?;
    let details = resp
        .publishedfiledetails
        .into_iter()
        .find(|d| d.publishedfileid == Some(collection_id))
        .with_context(|| format!("Steam returned nothing for {collection_id}"))?;
    collection_details_from(details)
}

/// The pure half of [`get_collection_details`], split out to be testable
/// without a Steam session.
fn collection_details_from(details: PublishedFileDetails) -> Result<CollectionDetails> {
    let id = details.publishedfileid.unwrap_or(0);
    let eresult = details.result.unwrap_or(0);
    if eresult != 1 {
        bail!(
            "Workshop item {id} isn't visible to the cluster's Steam account (eresult {eresult}) \
             -- removed, private, or not a real id"
        );
    }
    if details.file_type.unwrap_or(0) != FILE_TYPE_COLLECTION {
        bail!("Workshop item {id} is not a collection");
    }
    Ok(CollectionDetails {
        id,
        title: details.title.clone().unwrap_or_default(),
        description: details.file_description.clone().unwrap_or_default(),
        visibility: details.visibility.and_then(Visibility::from_steam),
        creator: details.creator.unwrap_or(0),
        time_updated: details.time_updated.unwrap_or(0),
        children: crate::steam::ordered_children(&details.children),
    })
}

/// Every Arma 3 collection `steam_id` has published, most recently
/// updated first.
pub async fn list_owned_collections(
    conn: &mut CmConnection,
    steam_id: u64,
) -> Result<Vec<CollectionSummary>> {
    let mut out = Vec::new();
    let mut page = 1;
    loop {
        let req = steamdepot::proto::CPublishedFileGetUserFilesRequest {
            steamid: Some(steam_id),
            appid: Some(ARMA3_APPID),
            page: Some(page),
            numperpage: Some(USER_FILES_PAGE_SIZE),
            r#type: Some("myfiles".to_string()),
            sortmethod: Some("lastupdated".to_string()),
            filetype: Some(MATCHING_FILE_TYPE_COLLECTIONS),
            return_children: Some(true),
            ..Default::default()
        };
        let resp: steamdepot::proto::CPublishedFileGetUserFilesResponse =
            call(conn, "PublishedFile.GetUserFiles#1", &req, true)
                .await
                .context("failed to list the account's Workshop collections")?;

        let got = resp.publishedfiledetails.len();
        out.extend(
            resp.publishedfiledetails
                .into_iter()
                .filter_map(collection_summary_from),
        );
        // Bounded by what Steam says it has, and by a short page -- either
        // alone could loop forever on a response that lies about the other.
        let total = resp.total.unwrap_or(0) as usize;
        if got < USER_FILES_PAGE_SIZE as usize || page as usize * USER_FILES_PAGE_SIZE as usize >= total {
            break;
        }
        page += 1;
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.time_updated));
    Ok(out)
}

/// One `GetUserFiles` result as a summary, or `None` for anything that
/// isn't a live collection.
fn collection_summary_from(details: PublishedFileDetails) -> Option<CollectionSummary> {
    if details.result.unwrap_or(0) != 1 || details.file_type.unwrap_or(0) != FILE_TYPE_COLLECTION {
        return None;
    }
    Some(CollectionSummary {
        id: details.publishedfileid?,
        title: details.title.clone().unwrap_or_default(),
        visibility: details.visibility.and_then(Visibility::from_steam),
        // `num_children` when Steam sends it; the children list otherwise.
        item_count: details
            .num_children
            .unwrap_or(details.children.len() as u32),
        time_updated: details.time_updated.unwrap_or(0),
    })
}

fn validate_title(title: &str) -> Result<&str> {
    let title = title.trim();
    if title.is_empty() {
        bail!("collection title must not be empty");
    }
    Ok(title)
}

/// Collapse duplicates (first occurrence wins, order kept -- a preset's
/// own order is meaningful to whoever exported it) and check the result
/// against Steam's limits before anything is sent.
fn validate_children(children: &[u64]) -> Result<Vec<u64>> {
    let mut seen = std::collections::HashSet::new();
    let children: Vec<u64> = children
        .iter()
        .copied()
        .filter(|id| seen.insert(*id))
        .collect();
    if children.is_empty() {
        bail!("refusing to set an empty membership on a collection");
    }
    if children.len() > MAX_CHILDREN {
        bail!(
            "collection would have {} members, over Steam's {MAX_CHILDREN} limit",
            children.len()
        );
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use steamdepot::proto::published_file_details::Child;

    use super::*;

    #[test]
    fn unlisted_is_the_default_visibility() {
        // The safety property the enum exists to hold: a caller that
        // never mentions visibility does not publish publicly.
        assert_eq!(Visibility::default(), Visibility::Unlisted);
        assert_eq!(Visibility::default().as_steam(), 3);
    }

    #[test]
    fn visibility_maps_to_steam_values_and_back() {
        for (v, steam) in [
            (Visibility::Public, 0),
            (Visibility::FriendsOnly, 1),
            (Visibility::Private, 2),
            (Visibility::Unlisted, 3),
        ] {
            assert_eq!(v.as_steam(), steam);
            assert_eq!(Visibility::from_steam(steam), Some(v));
        }
        assert_eq!(Visibility::from_steam(4), None);
    }

    #[test]
    fn proto_visibility_roundtrips_and_rejects_unset() {
        for v in [
            Visibility::Public,
            Visibility::FriendsOnly,
            Visibility::Private,
            Visibility::Unlisted,
        ] {
            assert_eq!(Visibility::from_proto(v.as_proto()).unwrap(), v);
        }
        // 0 is proto3's "field absent", and must not fall through to
        // Public -- that would make forgetting the field publish.
        assert!(Visibility::from_proto(0).is_err());
        assert!(Visibility::from_proto(99).is_err());
        assert!(Visibility::from_proto(-1).is_err());
    }

    #[test]
    fn proto_numbering_is_steam_shifted_by_one() {
        // The two protos document this; pinned so neither side drifts.
        assert_eq!(Visibility::Public.as_proto(), 1);
        assert_eq!(Visibility::Unlisted.as_proto(), 4);
    }

    #[test]
    fn children_are_deduped_keeping_first_occurrence_order() {
        assert_eq!(
            validate_children(&[450814997, 623475643, 450814997, 1779063631]).unwrap(),
            vec![450814997, 623475643, 1779063631]
        );
    }

    #[test]
    fn empty_membership_is_refused() {
        // Would silently wipe an existing collection.
        assert!(validate_children(&[]).is_err());
    }

    #[test]
    fn membership_over_steams_limit_is_refused_with_the_limit_named() {
        let ids: Vec<u64> = (1..=MAX_CHILDREN as u64 + 1).collect();
        let err = validate_children(&ids).unwrap_err();
        assert!(format!("{err}").contains("1000"), "{err}");
        // Exactly at the limit is fine, and duplicates don't count twice.
        let mut at_limit: Vec<u64> = (1..=MAX_CHILDREN as u64).collect();
        at_limit.push(1);
        assert_eq!(validate_children(&at_limit).unwrap().len(), MAX_CHILDREN);
    }

    #[test]
    fn blank_titles_are_refused_and_titles_trimmed() {
        assert!(validate_title("   ").is_err());
        assert_eq!(validate_title("  Sahatra  ").unwrap(), "Sahatra");
    }

    fn child(id: u64, sortorder: Option<u32>) -> Child {
        Child {
            publishedfileid: Some(id),
            sortorder,
            file_type: Some(0),
        }
    }

    fn collection(id: u64) -> PublishedFileDetails {
        PublishedFileDetails {
            result: Some(1),
            publishedfileid: Some(id),
            file_type: Some(FILE_TYPE_COLLECTION),
            title: Some("Sahatra ops".into()),
            file_description: Some("Thursday".into()),
            visibility: Some(3),
            creator: Some(76561198000000001),
            time_updated: Some(1_790_000_000),
            ..Default::default()
        }
    }

    #[test]
    fn details_carry_members_in_collection_sort_order() {
        let mut d = collection(42);
        d.children = vec![child(30, Some(2)), child(10, Some(0)), child(20, Some(1))];
        let details = collection_details_from(d).unwrap();
        assert_eq!(details.children, vec![10, 20, 30]);
        assert_eq!(details.title, "Sahatra ops");
        assert_eq!(details.description, "Thursday");
        assert_eq!(details.visibility, Some(Visibility::Unlisted));
        assert_eq!(details.creator, 76561198000000001);
    }

    #[test]
    fn details_refuse_a_mod() {
        // A mod's children are its Required Items, not membership.
        let mut d = collection(42);
        d.file_type = Some(0);
        d.children = vec![child(450814997, Some(0))];
        let err = collection_details_from(d).unwrap_err();
        assert!(format!("{err}").contains("not a collection"), "{err}");
    }

    #[test]
    fn details_refuse_an_item_steam_wont_show() {
        let mut d = collection(42);
        d.result = Some(9); // FileNotFound
        let err = collection_details_from(d).unwrap_err();
        assert!(format!("{err}").contains("isn't visible"), "{err}");
    }

    #[test]
    fn summaries_skip_anything_but_live_collections() {
        let mut a_mod = collection(1);
        a_mod.file_type = Some(0);
        let mut gone = collection(2);
        gone.result = Some(9);
        assert_eq!(collection_summary_from(a_mod), None);
        assert_eq!(collection_summary_from(gone), None);

        let mut live = collection(3);
        live.children = vec![child(10, Some(0)), child(11, Some(1))];
        let summary = collection_summary_from(live).unwrap();
        assert_eq!(summary.id, 3);
        assert_eq!(summary.item_count, 2);
        assert_eq!(summary.visibility, Some(Visibility::Unlisted));
    }

    #[test]
    fn summary_prefers_steams_own_child_count() {
        // GetUserFiles can report a count without listing every child.
        let mut live = collection(3);
        live.num_children = Some(115);
        assert_eq!(collection_summary_from(live).unwrap().item_count, 115);
    }
}
