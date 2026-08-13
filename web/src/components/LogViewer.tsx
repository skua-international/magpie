// Log popup with regex filtering.
//
// Renders structured lines structurally: launcher logs through
// tracing_subscriber's JSON layer, so most lines are objects carrying a
// timestamp, level, message and arbitrary extra fields -- and
// arma3server's own output is forwarded through the same layer with a
// `stream` field saying whether it came from stdout or stderr. But not
// everything is JSON: anything the process writes before tracing is set
// up, or that arma prints outside the forwarder, arrives as plain text.
// So each line is parsed independently and falls back to raw rather than
// the viewer assuming one shape for the whole stream.

import { useMemo, useState } from "react";

import { servers } from "../api/clients";
import { Banner, Button, Field, Spinner } from "./ui";
import { useAsync } from "./useAsync";

interface ParsedLine {
  raw: string;
  timestamp?: string;
  level?: string;
  message?: string;
  stream?: string;
  /// Everything that isn't one of the known keys, kept so a structured
  /// line's extra context isn't silently dropped.
  fields?: Record<string, unknown>;
}

/// tracing's JSON layer nests user fields under "fields" and puts the
/// message inside it, but flattens some shapes -- both are handled rather
/// than assuming one.
function parseLine(raw: string): ParsedLine {
  const trimmed = raw.trim();
  if (!trimmed.startsWith("{")) return { raw };
  try {
    const obj = JSON.parse(trimmed) as Record<string, unknown>;
    const nested = (obj.fields ?? {}) as Record<string, unknown>;
    const known = new Set([
      "timestamp",
      "level",
      "message",
      "fields",
      "target",
      "stream",
    ]);
    const extra: Record<string, unknown> = {};
    for (const [k, v] of Object.entries({ ...nested, ...obj })) {
      if (!known.has(k) && k !== "message") extra[k] = v;
    }
    return {
      raw,
      timestamp: typeof obj.timestamp === "string" ? obj.timestamp : undefined,
      level: typeof obj.level === "string" ? obj.level : undefined,
      message:
        typeof nested.message === "string"
          ? nested.message
          : typeof obj.message === "string"
            ? obj.message
            : undefined,
      stream:
        typeof obj.stream === "string"
          ? obj.stream
          : typeof nested.stream === "string"
            ? nested.stream
            : undefined,
      fields: Object.keys(extra).length > 0 ? extra : undefined,
    };
  } catch {
    // Valid-looking but unparseable: still a log line, show it as one.
    return { raw };
  }
}

export function LogViewer({
  serverId,
  serverName,
  onClose,
}: {
  serverId: string;
  serverName: string;
  onClose: () => void;
}) {
  const [tail, setTail] = useState(500);
  const [previous, setPrevious] = useState(false);
  const [pattern, setPattern] = useState("");

  const logs = useAsync(
    () => servers.getServerLogs({ id: serverId, tailLines: tail, previous }),
    [serverId, tail, previous],
  );

  const parsed = useMemo(
    () => (logs.data?.lines ?? []).map(parseLine),
    [logs.data],
  );

  // An in-progress regex is usually an invalid one ("[", "foo("), so a
  // parse failure is reported next to the box rather than thrown -- the
  // list just stops filtering until the pattern is valid again.
  const { filtered, regexError } = useMemo(() => {
    if (!pattern) return { filtered: parsed, regexError: null as string | null };
    let re: RegExp;
    try {
      re = new RegExp(pattern, "i");
    } catch (err) {
      return { filtered: parsed, regexError: (err as Error).message };
    }
    // Matched against the raw line, so a pattern can hit anything --
    // message text, a field value, or JSON keys -- rather than only
    // whatever this viewer chose to render.
    return { filtered: parsed.filter((l) => re.test(l.raw)), regexError: null };
  }, [parsed, pattern]);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      {/* Clicks inside must not reach the backdrop's close handler. */}
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <header className="page-header">
          <h3>
            Logs — {serverName}
            {logs.data?.podName && (
              <span className="muted mono"> {logs.data.podName}</span>
            )}
          </h3>
          <Button onClick={onClose}>Close</Button>
        </header>

        <div className="log-controls">
          <Field label="Filter (regex)">
            <input
              value={pattern}
              placeholder="e.g. ERROR|WARN, or steam.*fail"
              onChange={(e) => setPattern(e.target.value)}
            />
          </Field>
          <Field label="Lines">
            <select value={tail} onChange={(e) => setTail(Number(e.target.value))}>
              {[100, 500, 2000, 5000].map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </Field>
          <label className="wildcard">
            <input
              type="checkbox"
              checked={previous}
              onChange={() => setPrevious((p) => !p)}
            />
            {/* The only way to see why a crashed server died. */}
            <span>Previous container</span>
          </label>
          <Button onClick={logs.reload}>Refresh</Button>
        </div>

        {regexError && <Banner kind="error">Invalid regex: {regexError}</Banner>}
        {logs.error && <Banner kind="error">{logs.error}</Banner>}
        {logs.loading && <Spinner label="Loading logs…" />}

        {!logs.loading && !logs.error && (
          <>
            <p className="muted">
              {filtered.length} of {parsed.length} lines
              {pattern && !regexError && " matching"}
            </p>
            <div className="log-body">
              {filtered.length === 0 ? (
                <p className="muted">No lines to show.</p>
              ) : (
                filtered.map((line, i) => <LogLine key={i} line={line} />)
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function LogLine({ line }: { line: ParsedLine }) {
  // Unparsed lines render as-is rather than being forced into columns
  // that would all be empty.
  if (!line.message && !line.level) {
    return <div className="log-line log-raw">{line.raw}</div>;
  }
  const level = (line.level ?? "").toLowerCase();
  return (
    <div className={`log-line log-${level}`}>
      {line.timestamp && <span className="log-ts">{line.timestamp}</span>}
      {line.level && <span className="log-level">{line.level}</span>}
      {line.stream && <span className="log-stream">{line.stream}</span>}
      <span className="log-msg">{line.message ?? line.raw}</span>
      {line.fields && (
        <span className="log-fields">
          {Object.entries(line.fields).map(([k, v]) => (
            <span key={k} className="log-field">
              {k}={typeof v === "string" ? v : JSON.stringify(v)}
            </span>
          ))}
        </span>
      )}
    </div>
  );
}
