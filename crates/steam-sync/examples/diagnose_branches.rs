//! Throwaway diagnostic: compare depot manifest resolution between the
//! "public" and "creatordlc" branches of app 233780 (Arma 3 Server), to
//! check whether "creatordlc" is missing base-game content that "public"
//! carries. Not part of the real build -- run with:
//!   cargo run --release -p steam-sync --example diagnose_branches

use steam_sync::steam::login_anonymous;
use steamdepot::depot;
use steamdepot::pics;

const APP_ID: u32 = 233780;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut conn = login_anonymous().await?;

    let info = pics::get_product_info(&mut conn, &[APP_ID], &[]).await?;
    let app_info = info.apps.into_iter().next().expect("app not found");

    for branch in ["public", "creatordlc"] {
        println!("\n=== branch: {branch} ===");
        let depots = depot::resolve_depots(&app_info, "linux", branch)?;
        let mut sorted = depots.clone();
        sorted.sort_by_key(|d| d.depot_id);
        for d in &sorted {
            println!(
                "  depot {:>7}  manifest_id {:<25} from_app {:?}",
                d.depot_id, d.manifest_id, d.depot_from_app
            );
        }
        println!("  ({} depots total)", sorted.len());
    }

    Ok(())
}
