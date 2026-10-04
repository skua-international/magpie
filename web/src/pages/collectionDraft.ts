// The editable state behind the Collections page's editor, as plain
// functions -- everything here decides what gets published under the
// cluster's Steam account, so it's kept testable apart from React.

import { CollectionVisibility } from "../../../generated/ts/registry/v1/registry_pb";

/// One collection member as the editor holds it. Workshop ids are uint64,
/// which Connect hands over as bigint -- kept that way rather than
/// converted to number, which loses precision past 2^53 (current ids are
/// ~3.8e9, but nothing promises they stay small).
export interface DraftItem {
  id: bigint;
  title: string;
}

/// Pull Workshop ids out of whatever was pasted: full Workshop links (mod
/// or collection), bare ids, or a mix, separated by whitespace or commas.
/// Anything that's neither is handed back rather than dropped, so a typo
/// gets pointed out instead of silently doing nothing.
export function parseWorkshopRefs(text: string): { ids: bigint[]; invalid: string[] } {
  const ids: bigint[] = [];
  const invalid: string[] = [];
  for (const token of text.split(/[\s,]+/).filter(Boolean)) {
    const id = /[?&]id=(\d+)/.exec(token)?.[1] ?? (/^\d+$/.test(token) ? token : null);
    if (id === null || BigInt(id) === 0n) {
      invalid.push(token);
      continue;
    }
    if (!ids.includes(BigInt(id))) ids.push(BigInt(id));
  }
  return { ids, invalid };
}

/// `existing` with every item of `added` it doesn't already hold appended,
/// in order. Existing items win, so re-importing a preset never reorders
/// or retitles what's already listed.
export function mergeItems(existing: DraftItem[], added: DraftItem[]): DraftItem[] {
  const seen = new Set(existing.map((i) => i.id));
  const out = [...existing];
  for (const item of added) {
    if (seen.has(item.id)) continue;
    seen.add(item.id);
    out.push(item);
  }
  return out;
}

/// What saving would change about a collection's membership, for the
/// summary shown next to the save button. Order-only changes don't
/// count -- Arma doesn't care what order a collection lists mods in.
export function membershipDiff(
  original: bigint[],
  current: DraftItem[],
): { added: number; removed: number } {
  const before = new Set(original);
  const after = new Set(current.map((i) => i.id));
  let added = 0;
  let removed = 0;
  for (const id of after) if (!before.has(id)) added++;
  for (const id of before) if (!after.has(id)) removed++;
  return { added, removed };
}

/// Items whose title or id contains `query`, case-insensitively. Exists
/// because finding the one mod to remove from a 115-row list by scrolling
/// is the main thing the editor is for.
export function filterItems(items: DraftItem[], query: string): DraftItem[] {
  const q = query.trim().toLowerCase();
  if (!q) return items;
  return items.filter((i) => i.title.toLowerCase().includes(q) || i.id.toString().includes(q));
}

/// A preset export's filename as a default collection title --
/// "BearzSahatra.html" -> "BearzSahatra". The Launcher names the file
/// after the preset, so this is usually exactly what the operator called
/// it.
export function titleFromFilename(name: string): string {
  return name.replace(/\.html?$/i, "").trim();
}

export const VISIBILITY_OPTIONS: { value: CollectionVisibility; label: string }[] = [
  { value: CollectionVisibility.UNLISTED, label: "Unlisted (anyone with the link)" },
  { value: CollectionVisibility.PUBLIC, label: "Public" },
  { value: CollectionVisibility.FRIENDS_ONLY, label: "Friends only" },
  { value: CollectionVisibility.PRIVATE, label: "Private" },
];

export function visibilityLabel(v: CollectionVisibility): string {
  switch (v) {
    case CollectionVisibility.PUBLIC:
      return "public";
    case CollectionVisibility.FRIENDS_ONLY:
      return "friends only";
    case CollectionVisibility.PRIVATE:
      return "private";
    case CollectionVisibility.UNLISTED:
      return "unlisted";
    default:
      return "unknown";
  }
}

/// The Workshop page for an id. Built from the id rather than taken from
/// anywhere user-supplied, so it is always a steamcommunity.com link.
export function workshopUrl(id: bigint): string {
  return `https://steamcommunity.com/sharedfiles/filedetails/?id=${id}`;
}
