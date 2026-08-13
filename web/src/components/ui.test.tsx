import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { MetadataChips, Reference, formatBytes, formatTimestamp, toEntries, toRecord } from "./ui";

// The security-relevant one. A mod source's `reference` is operator-
// supplied (a Steam URL, a preset URL, a local id), and turning it into
// an <a href> without checking the scheme would make a stored
// javascript:/data: reference executable on click.
describe("Reference", () => {
  it("links http and https references", () => {
    for (const url of [
      "https://steamcommunity.com/sharedfiles/filedetails/?id=463939057",
      "http://example.com/preset.html",
    ]) {
      const { unmount } = render(<Reference reference={url} />);
      const link = screen.getByRole("link", { name: url });
      expect(link).toHaveProperty("href");
      // Opened in a new tab, so noopener matters: without it the opened
      // page gets a handle on this one via window.opener.
      expect(link.getAttribute("rel")).toContain("noopener");
      unmount();
    }
  });

  it("does not link a javascript: or data: reference", () => {
    for (const hostile of [
      "javascript:alert(document.cookie)",
      "data:text/html,<script>alert(1)</script>",
      // Uppercase scheme, in case the check ever becomes a string
      // comparison against a lowercase literal.
      "JAVASCRIPT:alert(1)",
    ]) {
      const { unmount } = render(<Reference reference={hostile} />);
      expect(screen.queryByRole("link")).toBeNull();
      expect(screen.getByText(hostile)).toBeTruthy();
      unmount();
    }
  });

  it("renders a non-URL reference as plain text", () => {
    // Local sources carry a caller-assigned id, and preset-from-content
    // sources carry the literal "(uploaded HTML)".
    for (const plain of ["skua_custom", "(uploaded HTML)"]) {
      const { unmount } = render(<Reference reference={plain} />);
      expect(screen.queryByRole("link")).toBeNull();
      expect(screen.getByText(plain)).toBeTruthy();
      unmount();
    }
  });
});

// These decide what actually gets sent to a replace-the-whole-set RPC,
// so a bug here silently drops or resurrects metadata.
describe("metadata helpers", () => {
  it("round-trips a record through entries", () => {
    const original = { owner: "ops-team", tier: "prod" };
    expect(toRecord(toEntries(original))).toEqual(original);
  });

  it("drops blank rows rather than sending an empty key", () => {
    // "Add metadata" appends an empty row; submitting before typing must
    // not fail the whole save on a key the server rejects.
    const entries = [
      { key: "owner", value: "ops" },
      { key: "   ", value: "ignored" },
      { key: "", value: "" },
    ];
    expect(toRecord(entries)).toEqual({ owner: "ops" });
  });

  it("trims keys but preserves values verbatim", () => {
    expect(toRecord([{ key: "  owner  ", value: "  spaced  " }])).toEqual({
      owner: "  spaced  ",
    });
  });

  it("keeps an explicitly empty value", () => {
    // Distinct from a blank row: the key was typed, so the entry is real.
    expect(toRecord([{ key: "flag", value: "" }])).toEqual({ flag: "" });
  });

  it("renders an em dash when there is no metadata", () => {
    render(<MetadataChips metadata={{}} />);
    expect(screen.getByText("—")).toBeTruthy();
  });

  it("renders one chip per entry", () => {
    render(<MetadataChips metadata={{ a: "1", b: "2" }} />);
    expect(screen.getByText("a=1")).toBeTruthy();
    expect(screen.getByText("b=2")).toBeTruthy();
  });
});

describe("formatBytes", () => {
  it("formats each unit", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1024)).toBe("1.0 KiB");
    expect(formatBytes(2216983552)).toBe("2.1 GiB");
  });

  it("accepts the bigint the proto actually delivers", () => {
    // size_bytes is uint64, so Connect hands back a bigint -- passing it
    // to a Number-only implementation would throw at runtime.
    expect(formatBytes(1073741824n)).toBe("1.0 GiB");
  });

  it("caps at the largest known unit rather than producing undefined", () => {
    expect(formatBytes(1024 ** 6)).toContain("TiB");
  });
});

describe("formatTimestamp", () => {
  it("renders an em dash for a missing timestamp", () => {
    // created_at_unix_ms is 0 for anything never persisted with one.
    expect(formatTimestamp(0)).toBe("—");
    expect(formatTimestamp(0n)).toBe("—");
  });

  it("formats a real timestamp", () => {
    expect(formatTimestamp(1754000000000)).not.toBe("—");
  });
});
