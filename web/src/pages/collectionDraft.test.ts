import { describe, expect, it } from "vitest";

import { CollectionVisibility } from "../../../generated/ts/registry/v1/registry_pb";
import {
  VISIBILITY_OPTIONS,
  filterItems,
  membershipDiff,
  mergeItems,
  parseWorkshopRefs,
  titleFromFilename,
  visibilityLabel,
  workshopUrl,
} from "./collectionDraft";

describe("parseWorkshopRefs", () => {
  it("accepts Workshop links, bare ids, and a mix", () => {
    const { ids, invalid } = parseWorkshopRefs(
      "https://steamcommunity.com/sharedfiles/filedetails/?id=450814997\n623475643, " +
        "http://steamcommunity.com/workshop/filedetails/?id=1779063631",
    );
    expect(ids).toEqual([450814997n, 623475643n, 1779063631n]);
    expect(invalid).toEqual([]);
  });

  it("finds the id when it isn't the first query parameter", () => {
    // Links copied out of the Steam client carry extra parameters.
    expect(
      parseWorkshopRefs("https://steamcommunity.com/sharedfiles/filedetails/?l=english&id=42").ids,
    ).toEqual([42n]);
  });

  it("hands back what it couldn't read instead of dropping it", () => {
    const { ids, invalid } = parseWorkshopRefs("450814997 cba https://example.com/x 0");
    expect(ids).toEqual([450814997n]);
    expect(invalid).toEqual(["cba", "https://example.com/x", "0"]);
  });

  it("collapses repeats", () => {
    expect(parseWorkshopRefs("5 5 https://steamcommunity.com/sharedfiles/filedetails/?id=5").ids).toEqual([5n]);
  });

  it("keeps ids past 2^53 exact", () => {
    // Number() would round this; the bigint must survive untouched.
    expect(parseWorkshopRefs("9007199254740993").ids).toEqual([9007199254740993n]);
  });

  it("treats blank input as nothing to do", () => {
    expect(parseWorkshopRefs("  \n , ")).toEqual({ ids: [], invalid: [] });
  });
});

describe("mergeItems", () => {
  const a = { id: 1n, title: "A" };
  const b = { id: 2n, title: "B" };

  it("appends new items in order", () => {
    expect(mergeItems([a], [b, { id: 3n, title: "C" }]).map((i) => i.id)).toEqual([1n, 2n, 3n]);
  });

  it("never duplicates, and keeps the existing entry", () => {
    // Re-importing the same preset must be a no-op, not a reshuffle.
    const merged = mergeItems([a, b], [{ id: 1n, title: "A (renamed)" }, b]);
    expect(merged).toEqual([a, b]);
  });

  it("collapses duplicates within the added batch too", () => {
    expect(mergeItems([], [a, a]).length).toBe(1);
  });
});

describe("membershipDiff", () => {
  it("counts additions and removals", () => {
    expect(
      membershipDiff([1n, 2n, 3n], [
        { id: 2n, title: "" },
        { id: 4n, title: "" },
      ]),
    ).toEqual({ added: 1, removed: 2 });
  });

  it("ignores reordering", () => {
    expect(
      membershipDiff([1n, 2n], [
        { id: 2n, title: "" },
        { id: 1n, title: "" },
      ]),
    ).toEqual({ added: 0, removed: 0 });
  });
});

describe("filterItems", () => {
  const items = [
    { id: 450814997n, title: "CBA_A3" },
    { id: 623475643n, title: "3den Enhanced" },
  ];

  it("matches title case-insensitively", () => {
    expect(filterItems(items, "cba").map((i) => i.title)).toEqual(["CBA_A3"]);
  });

  it("matches by id", () => {
    expect(filterItems(items, "6234").map((i) => i.title)).toEqual(["3den Enhanced"]);
  });

  it("returns everything for a blank query", () => {
    expect(filterItems(items, "  ")).toBe(items);
  });
});

describe("titleFromFilename", () => {
  it("strips the export's extension", () => {
    expect(titleFromFilename("BearzSahatra.html")).toBe("BearzSahatra");
    expect(titleFromFilename("Ops.HTM")).toBe("Ops");
  });

  it("leaves other names alone", () => {
    expect(titleFromFilename("preset.html.bak")).toBe("preset.html.bak");
  });
});

describe("visibility", () => {
  it("offers Unlisted first, so it's what a new collection starts on", () => {
    // The safe default: shareable by link, not in public Workshop search.
    expect(VISIBILITY_OPTIONS[0].value).toBe(CollectionVisibility.UNLISTED);
  });

  it("never offers UNSPECIFIED, which the server refuses", () => {
    expect(VISIBILITY_OPTIONS.map((o) => o.value)).not.toContain(CollectionVisibility.UNSPECIFIED);
    expect(VISIBILITY_OPTIONS).toHaveLength(4);
  });

  it("labels every value, and an unknown one as such", () => {
    expect(visibilityLabel(CollectionVisibility.PUBLIC)).toBe("public");
    expect(visibilityLabel(CollectionVisibility.FRIENDS_ONLY)).toBe("friends only");
    expect(visibilityLabel(CollectionVisibility.PRIVATE)).toBe("private");
    expect(visibilityLabel(CollectionVisibility.UNLISTED)).toBe("unlisted");
    expect(visibilityLabel(CollectionVisibility.UNSPECIFIED)).toBe("unknown");
  });
});

describe("workshopUrl", () => {
  it("builds a steamcommunity.com link from the id", () => {
    expect(workshopUrl(3813510055n)).toBe(
      "https://steamcommunity.com/sharedfiles/filedetails/?id=3813510055",
    );
  });
});
