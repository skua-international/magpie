//! Extract *candidate* Steam Workshop IDs from whatever a caller was given
//! -- a preset HTML export (many mods) or a single raw Workshop URL (one
//! mod or collection, indistinguishable from the URL alone). Deliberately
//! does no Steam API calls of any kind: mod-vs-collection detection and
//! collection expansion need an authenticated Steam session to correctly
//! respect private/unlisted visibility, which lives in `steam-sync`
//! (`resolve_source_ids`), not here.

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_9_3) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/35.0.1916.47 Safari/537.36";

static FILEDETAILS_ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"filedetails/\?id=(\d+)").unwrap());

/// Extract candidate IDs from `mod_source`, auto-detecting which of the two
/// supported shapes it is:
///
/// - A single raw Workshop URL (`.../filedetails/?id=<id>`, mod or
///   collection) -- the ID is embedded in `mod_source` itself, so this
///   returns it directly with no network call at all.
/// - Anything else -- treated as a preset HTML export, fetched (over HTTP
///   if `mod_source` looks like a URL, read from disk otherwise) and
///   scanned for every `filedetails/?id=` link it contains.
pub async fn extract_candidate_ids(mod_source: &str) -> Result<Vec<u64>> {
    if let Some(id) = extract_single_id(mod_source) {
        return Ok(vec![id]);
    }

    let html = fetch_source(mod_source).await?;
    Ok(parse_preset_html(&html))
}

/// If `url` itself is a single Workshop `filedetails` link, return its ID.
/// Public because callers that already have a single raw Workshop URL in
/// hand (no preset export, no fetch needed) can skip straight to this.
pub fn extract_single_id(url: &str) -> Option<u64> {
    FILEDETAILS_ID_RE
        .captures(url)
        .and_then(|c| c[1].parse().ok())
}

/// Scan `html` for every `filedetails/?id=` link (a preset export's usual
/// shape -- one row per mod). Public for callers that already have preset
/// HTML content in hand (e.g. uploaded directly rather than fetched from a
/// URL) and want to skip [`extract_candidate_ids`]'s own fetch step.
pub fn parse_preset_html(html: &str) -> Vec<u64> {
    FILEDETAILS_ID_RE
        .captures_iter(html)
        .filter_map(|c| c[1].parse().ok())
        .collect()
}

async fn fetch_source(mod_source: &str) -> Result<String> {
    if mod_source.starts_with("http") {
        let client = reqwest::Client::new();
        client
            .get(mod_source)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .context("failed to fetch mod preset URL")?
            .text()
            .await
            .context("failed to read mod preset response body")
    } else {
        std::fs::read_to_string(mod_source)
            .with_context(|| format!("failed to read mod preset file {mod_source}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_url_extracts_directly() {
        assert_eq!(
            extract_single_id("https://steamcommunity.com/sharedfiles/filedetails/?id=1978754337"),
            Some(1978754337)
        );
        assert_eq!(
            extract_single_id("https://steamcommunity.com/workshop/filedetails/?id=843770737"),
            Some(843770737)
        );
    }

    #[test]
    fn preset_html_extracts_all() {
        let html = r#"
            <a href="https://steamcommunity.com/sharedfiles/filedetails/?id=843425103">a</a>
            <a href="https://steamcommunity.com/sharedfiles/filedetails/?id=843593391">b</a>
        "#;
        let mut ids = parse_preset_html(html);
        ids.sort();
        assert_eq!(ids, vec![843425103, 843593391]);
    }
}
