// Per-server health cell.
//
// Fetched once when the row mounts, with an explicit re-check button --
// deliberately not polled. Each call reads a pod through the API server,
// so a fleet page polling every row would generate constant load for a
// signal that changes on the scale of a server restart. An operator
// watching a server come up can press the button.

import { servers } from "../api/clients";
import { Button } from "./ui";
import { useAsync } from "./useAsync";

export function ServerHealth({ serverId }: { serverId: string }) {
  const health = useAsync(() => servers.getServerHealth({ id: serverId }), [serverId]);

  if (health.loading) return <span className="muted">checking…</span>;
  // A failed health call is about this one cell, not the page -- the row
  // is still valid and every other action on it still works.
  if (health.error) {
    return (
      <span className="health">
        <span className="muted" title={health.error}>
          unavailable
        </span>
        <Button size="compact" onClick={health.reload}>↻</Button>
      </span>
    );
  }

  const data = health.data!;
  // "ready" is the A2S query probe passing, which is a stronger claim
  // than the pod merely running -- so an unready pod says why rather
  // than just showing a phase.
  const label = data.ready ? "ready" : data.phase || "not ready";
  const title = [
    data.message,
    data.podName && `pod: ${data.podName}`,
    data.restartCount > 0 && `restarts: ${data.restartCount}`,
  ]
    .filter(Boolean)
    .join("\n");

  return (
    <span className="health" title={title || undefined}>
      <span className={data.ready ? "phase-running" : "phase-stopped"}>{label}</span>
      {/* Surfaced rather than buried in the tooltip: a server that is
          ready but has restarted repeatedly is not actually healthy. */}
      {data.restartCount > 0 && (
        <span className="muted"> ×{data.restartCount}</span>
      )}
      {/* A glyph rather than "Re-check": this sits in every row, and the
          word is what made the health column wide enough to push the
          actions off-screen. */}
      <Button size="compact" onClick={health.reload}>
        ↻
      </Button>
    </span>
  );
}
