import { describe, expect, it } from "vitest";

import { parseLine } from "./LogViewer";

// The log stream is genuinely mixed -- launcher emits JSON through
// tracing's json layer, arma3server's own pre-tracing output does not --
// so the parser has to handle both without assuming one shape for the
// whole stream. Getting this wrong makes a real server's logs unreadable
// exactly when someone is trying to debug it.
describe("parseLine", () => {
  it("parses a tracing JSON line with fields-nested message", () => {
    const line = parseLine(
      JSON.stringify({
        timestamp: "2026-08-13T04:46:46.250Z",
        level: "INFO",
        fields: { message: "Starting Arma 3 Server..." },
      }),
    );
    expect(line.level).toBe("INFO");
    expect(line.message).toBe("Starting Arma 3 Server...");
    expect(line.timestamp).toBe("2026-08-13T04:46:46.250Z");
  });

  it("parses a top-level message too", () => {
    // Not every emitter nests it, so both shapes are accepted.
    const line = parseLine(JSON.stringify({ level: "WARN", message: "flat" }));
    expect(line.message).toBe("flat");
  });

  it("keeps the stream tag arma's forwarded output carries", () => {
    const line = parseLine(
      JSON.stringify({ level: "ERROR", stream: "stderr", fields: { message: "boom" } }),
    );
    expect(line.stream).toBe("stderr");
  });

  it("keeps extra fields rather than dropping them", () => {
    // The extra context is often the whole reason a line is useful.
    const line = parseLine(
      JSON.stringify({
        level: "INFO",
        fields: { message: "Mission read.", mission: "Kapaulio", attempt: 2 },
      }),
    );
    expect(line.fields).toMatchObject({ mission: "Kapaulio", attempt: 2 });
    // The message is rendered on its own, so it must not be duplicated
    // into the fields blob.
    expect(line.fields).not.toHaveProperty("message");
  });

  it("treats non-JSON output as a raw line", () => {
    for (const raw of [
      "17:04:11 Steam Manager initialized.",
      "==========================================",
      "",
    ]) {
      const line = parseLine(raw);
      expect(line.raw).toBe(raw);
      expect(line.message).toBeUndefined();
      expect(line.level).toBeUndefined();
    }
  });

  it("falls back to raw for something that starts like JSON but isn't", () => {
    // A truncated line is the realistic case: tail can cut mid-object.
    const truncated = '{"timestamp":"2026-08-13T04:46:46.250Z","level":"IN';
    const line = parseLine(truncated);
    expect(line.raw).toBe(truncated);
    expect(line.message).toBeUndefined();
  });

  it("survives JSON whose fields are the wrong types", () => {
    // Nothing guarantees a log line's shape, and a crash here would take
    // out the whole viewer rather than one line.
    const line = parseLine(JSON.stringify({ level: 42, message: { nested: true } }));
    expect(line.level).toBeUndefined();
    expect(line.message).toBeUndefined();
    expect(line.raw).toContain("42");
  });

  it("does not treat a JSON array as a structured line", () => {
    expect(parseLine('["not", "an", "object"]').message).toBeUndefined();
  });
});

// Mirrors the filtering the viewer does, which is the behaviour a user
// actually exercises while hunting through a log.
describe("regex filtering", () => {
  const lines = [
    JSON.stringify({ level: "INFO", fields: { message: "Logged in to Steam" } }),
    JSON.stringify({ level: "ERROR", fields: { message: "Cannot open object" } }),
    "17:05:02 Game started.",
  ].map(parseLine);

  function filter(pattern: string) {
    const re = new RegExp(pattern, "i");
    return lines.filter((l) => re.test(l.raw));
  }

  it("matches structured and raw lines alike", () => {
    expect(filter("game started")).toHaveLength(1);
    expect(filter("steam")).toHaveLength(1);
  });

  it("matches on level, which only exists in the raw JSON", () => {
    // Filtering against the raw line rather than the rendered message is
    // what makes this work.
    expect(filter("ERROR")).toHaveLength(1);
  });

  it("supports alternation, the common operator filter", () => {
    expect(filter("ERROR|WARN")).toHaveLength(1);
  });

  it("is case-insensitive", () => {
    expect(filter("cannot open")).toHaveLength(1);
  });

  it("an invalid pattern throws, which the viewer reports instead of filtering", () => {
    // Half-typed patterns ("[", "foo(") are the normal state while
    // typing, so the component catches this rather than unmounting.
    expect(() => new RegExp("[", "i")).toThrow();
  });
});
