//! Live check of the Workshop collection calls in `steam_sync::collection`
//! against real Steam, as a real account. Everything here is otherwise
//! only unit-tested against hand-built responses -- this is the one place
//! that confirms Steam actually accepts a collection `Publish` over a CM
//! session, and that `GetUserFiles`' collection filter is the right one.
//!
//! Publishes a *private* collection holding two public mods, reads it
//! back, lists the account's collections, edits it, then deletes it. It
//! cleans up after itself even when a step in between fails; if it can't,
//! it prints the id to delete by hand.
//!
//!   STEAM_USER=... STEAM_REFRESH_TOKEN=... \
//!     cargo run -p steam-sync --example collection_probe

use anyhow::{Context, Result, bail, ensure};
use steam_sync::collection::{self, Visibility};
use steam_sync::steam::{self, CmPool, SteamAuth};

/// CBA_A3 and 3den Enhanced: public, long-lived, and in most presets.
const MODS: [u64; 2] = [450814997, 623475643];

#[tokio::main]
async fn main() -> Result<()> {
    let user = std::env::var("STEAM_USER").context("STEAM_USER not set")?;
    let refresh_token =
        std::env::var("STEAM_REFRESH_TOKEN").context("STEAM_REFRESH_TOKEN not set")?;
    let scratch = tempfile::tempdir()?;
    let pool = CmPool::start(
        1,
        &SteamAuth::Session {
            user,
            refresh_token,
        },
        scratch.path(),
    )
    .await?;
    let mut conn = pool.acquire().await;
    let steam_id = collection::account_steam_id(&conn)?;
    println!("logged in as {steam_id}");

    let id = collection::publish_collection(
        &mut conn,
        "magpie collection probe",
        "Created by magpie's collection_probe example; deleted again when it finishes.",
        Visibility::Private,
    )
    .await?;
    println!("published {id}: https://steamcommunity.com/sharedfiles/filedetails/?id={id}");

    let outcome = exercise(&mut conn, steam_id, id).await;
    let cleanup = collection::delete_collection(&mut conn, id).await;
    match (&outcome, &cleanup) {
        (_, Err(e)) => println!("!! could not delete {id}, remove it by hand: {e:#}"),
        (_, Ok(())) => println!("deleted {id}"),
    }
    outcome?;
    cleanup?;

    // Deleting should make it disappear from the account's own listing.
    let after = collection::list_owned_collections(&mut conn, steam_id).await?;
    ensure!(
        after.iter().all(|c| c.id != id),
        "deleted collection {id} still listed"
    );
    println!("\nall collection calls behaved as expected");
    Ok(())
}

async fn exercise(conn: &mut steam::CmConnection, steam_id: u64, id: u64) -> Result<()> {
    collection::set_collection_children(conn, id, &MODS).await?;

    let details = collection::get_collection_details(conn, id).await?;
    println!("read back: {details:?}");
    ensure!(details.creator == steam_id, "creator {} != us", details.creator);
    ensure!(details.children == MODS, "children {:?} != {MODS:?}", details.children);
    ensure!(details.visibility == Some(Visibility::Private), "visibility {:?}", details.visibility);

    let resolved = steam::resolve_source_ids(conn, &details.children).await?;
    let titles: Vec<_> = resolved.mods.iter().map(|m| m.title.as_str()).collect();
    println!("members resolve to {titles:?}");

    // Steam's own listing can lag a fresh publish by a little.
    let mut listed = false;
    for _ in 0..5 {
        let owned = collection::list_owned_collections(conn, steam_id).await?;
        println!("account lists {} collection(s)", owned.len());
        if let Some(found) = owned.iter().find(|c| c.id == id) {
            println!("listed as: {found:?}");
            listed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    if !listed {
        bail!("GetUserFiles never listed {id} -- check MATCHING_FILE_TYPE_COLLECTIONS");
    }

    // Reverse the membership and rename, then confirm both stuck.
    let reversed: Vec<u64> = MODS.iter().rev().copied().collect();
    collection::set_collection_children(conn, id, &reversed).await?;
    collection::update_collection(
        conn,
        id,
        "magpie collection probe (edited)",
        "edited",
        Visibility::Private,
    )
    .await?;
    let edited = collection::get_collection_details(conn, id).await?;
    ensure!(edited.children == reversed, "reorder didn't stick: {:?}", edited.children);
    ensure!(edited.title == "magpie collection probe (edited)", "title {:?}", edited.title);
    println!("edit stuck: {:?} {:?}", edited.title, edited.children);
    Ok(())
}
