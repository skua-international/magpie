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

/// The Workshop page for `id` -- the inverse of [`extract_single_id`].
/// One place for it, so every service hands out the same link for the
/// same item rather than each rebuilding it.
pub fn workshop_url(id: u64) -> String {
    format!("https://steamcommunity.com/sharedfiles/filedetails/?id={id}")
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

/// Render `mods` (Workshop id, display name) as an Arma 3 Launcher preset
/// export named `name` -- the file the Launcher's MODS > PRESET > IMPORT
/// takes, and the one [`parse_preset_html`] reads.
///
/// Follows the Launcher's own export markup row for row (the
/// `data-type` attributes are what an importer keys on), with
/// `arma:Type` "preset" and an `arma:PresetName` so the import lands as a
/// named preset rather than an anonymous mod list. Rows go out in the
/// order given; the Launcher's own exports are alphabetical, and callers
/// wanting that sort first.
///
/// Every piece of text is escaped: display names are whatever a Workshop
/// author typed, and this file gets opened in browsers as well as the
/// Launcher.
pub fn render_preset_html(name: &str, mods: &[(u64, &str)]) -> String {
    let name = escape_html(name);
    let mut rows = String::new();
    for (id, title) in mods {
        let url = workshop_url(*id);
        rows.push_str(&format!(
            r#"        <tr data-type="ModContainer">
          <td data-type="DisplayName">{title}</td>
          <td>
            <span class="from-steam">Steam</span>
          </td>
          <td>
            <a href="{url}" data-type="Link">{url}</a>
          </td>
        </tr>
"#,
            title = escape_html(title),
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<html>
  <!--Exported by magpie in the Arma 3 Launcher's preset format.-->
  <head>
    <meta name="arma:Type" content="preset" />
    <meta name="arma:PresetName" content="{name}" />
    <meta name="generator" content="magpie" />
    <title>Arma 3</title>
    <style>
body {{ margin: 0; padding: 0; color: #fff; background: #000; }}
body, th, td {{ font: 95%/1.3 Roboto, Segoe UI, Tahoma, Arial, Helvetica, sans-serif; }}
td {{ padding: 3px 30px 3px 0; }}
h1 {{ padding: 20px 20px 0 20px; color: white; font-weight: 200; font-family: segoe ui; font-size: 3em; margin: 0; }}
em {{ font-variant: italic; color: silver; }}
.before-list {{ padding: 5px 20px 10px 20px; }}
.mod-list {{ background: #222222; padding: 20px; }}
.footer {{ padding: 20px; color: gray; }}
a {{ color: #D18F21; text-decoration: underline; }}
a:hover {{ color: #F1AF41; text-decoration: none; }}
.from-steam {{ color: #449EBD; }}
    </style>
  </head>
  <body>
    <h1>Arma 3  - Preset <strong>{name}</strong></h1>
    <p class="before-list">
      <em>To import this preset, drag this file onto the Launcher window. Or click the MODS tab, then PRESET in the top right, then IMPORT at the bottom, and finally select this file.</em>
    </p>
    <div class="mod-list">
      <table>
{rows}      </table>
    </div>
    <div class="footer">
      <span>{count} mods. Exported by magpie.</span>
    </div>
  </body>
</html>
"#,
        count = mods.len(),
    )
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
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
    fn workshop_url_round_trips_through_extract() {
        let url = workshop_url(3792213005);
        assert_eq!(
            url,
            "https://steamcommunity.com/sharedfiles/filedetails/?id=3792213005"
        );
        assert_eq!(extract_single_id(&url), Some(3792213005));
    }

    #[test]
    fn rendered_preset_parses_back_to_the_same_ids_in_order() {
        let mods = [
            (450814997, "CBA_A3"),
            (623475643, "3den Enhanced"),
            (3019928771, "Sa'hatra"),
        ];
        let html = render_preset_html("Bearz Sahatra", &mods);
        // Each mod is linked twice (href and text), exactly as the
        // Launcher's own export does.
        let ids = parse_preset_html(&html);
        assert_eq!(ids.len(), 6);
        let mut seen = std::collections::HashSet::new();
        let distinct: Vec<u64> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
        assert_eq!(distinct, vec![450814997, 623475643, 3019928771]);
    }

    #[test]
    fn rendered_preset_is_a_named_launcher_preset() {
        let html = render_preset_html("Thursday Ops", &[(450814997, "CBA_A3")]);
        assert!(html.contains(r#"<meta name="arma:Type" content="preset" />"#));
        assert!(html.contains(r#"<meta name="arma:PresetName" content="Thursday Ops" />"#));
        assert!(html.contains(r#"<td data-type="DisplayName">CBA_A3</td>"#));
        assert!(html.contains(r#"data-type="Link">https://steamcommunity.com/sharedfiles/filedetails/?id=450814997</a>"#));
    }

    #[test]
    fn rendered_preset_escapes_titles_and_name() {
        // Workshop titles are author-controlled, and the file is opened in
        // browsers as well as the Launcher.
        let html = render_preset_html(
            r#"Ops" onload="alert(1)"#,
            &[(1, "<script>alert('x')</script> & co")],
        );
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; co"));
        assert!(html.contains(r#"content="Ops&quot; onload=&quot;alert(1)""#));
    }

    #[test]
    fn rendered_preset_parses_the_same_as_a_real_launcher_export_would() {
        // Same rows the Launcher writes, so a magpie export of an import
        // is the identity on the mod list.
        let real = r#"<tr data-type="ModContainer">
          <td data-type="DisplayName">CBA_A3</td>
          <td>
            <span class="from-steam">Steam</span>
          </td>
          <td>
            <a href="https://steamcommunity.com/sharedfiles/filedetails/?id=450814997" data-type="Link">https://steamcommunity.com/sharedfiles/filedetails/?id=450814997</a>
          </td>
        </tr>"#;
        let ours = render_preset_html("x", &[(450814997, "CBA_A3")]);
        assert!(
            ours.contains(real),
            "row markup drifted from the Launcher's"
        );
    }

    #[test]
    fn empty_preset_still_renders() {
        let html = render_preset_html("Empty", &[]);
        assert!(parse_preset_html(&html).is_empty());
        assert!(html.contains("0 mods"));
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
