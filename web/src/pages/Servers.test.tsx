import { describe, expect, it } from "vitest";

import { danglingSelection } from "./Servers";

// Deleting a ModSource does not rewrite the ArmaServers referencing it,
// so a server's spec can name sources that no longer exist. Those ids
// seed the edit form's selection but get no checkbox (the picker renders
// only sources that exist), so nothing in the UI could clear them and
// every save resubmitted them -- UpdateServer then rejected the whole
// edit with "no such mod source", including the edit that would have
// removed the dead reference. Filtering them out on save is what makes
// the form able to repair the spec.
describe("danglingSelection", () => {
  const sources = [{ id: "live-a" }, { id: "live-b" }];

  it("finds selected ids with no registered source", () => {
    expect(danglingSelection(["live-a", "deleted"], sources)).toEqual(["deleted"]);
  });

  it("returns nothing when every selection still exists", () => {
    expect(danglingSelection(["live-a", "live-b"], sources)).toEqual([]);
    expect(danglingSelection([], sources)).toEqual([]);
  });

  it("treats every id as dangling when no sources are registered", () => {
    // The real shape of the bug that prompted this: the operator had
    // deleted the only source the server referenced.
    expect(danglingSelection(["deleted"], [])).toEqual(["deleted"]);
  });

  it("keeps live ids out of the dangling set regardless of order", () => {
    expect(danglingSelection(["gone-1", "live-b", "gone-2", "live-a"], sources)).toEqual([
      "gone-1",
      "gone-2",
    ]);
  });

  // The complement is what actually gets submitted, so it has to be
  // exactly the selection minus the dangling ids -- not a re-derivation
  // from `sources`, which would silently reorder or add.
  it("complements to the selection that is safe to submit", () => {
    const selected = ["gone", "live-b", "live-a"];
    const dangling = danglingSelection(selected, sources);
    expect(selected.filter((id) => !dangling.includes(id))).toEqual(["live-b", "live-a"]);
  });
});
