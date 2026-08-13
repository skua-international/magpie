// Server lifecycle: the ArmaServer CRUD magpiectl's `servers` commands
// and the TUI's server screen already cover.

import { useState } from "react";

import {
  DesiredState,
  ServerPhase,
} from "../../../generated/ts/controller/v1/controller_pb";
import { ModSourceKind } from "../../../generated/ts/registry/v1/registry_pb";
import { modSources, servers } from "../api/clients";
import {
  Banner,
  Button,
  Field,
  MetadataEditor,
  ModSourcePicker,
  toEntries,
  toRecord,
  Spinner,
  Table,
  confirmed,
} from "../components/ui";
import { LogViewer } from "../components/LogViewer";
import { ServerHealth } from "../components/ServerHealth";
import { useAction, useAsync } from "../components/useAsync";



function kindLabel(kind: ModSourceKind): string {
  switch (kind) {
    case ModSourceKind.MOD:
      return "mod";
    case ModSourceKind.COLLECTION:
      return "collection";
    case ModSourceKind.LOCAL:
      return "local";
    case ModSourceKind.PRESET:
      return "preset";
    default:
      return "unknown";
  }
}

function phaseLabel(phase: ServerPhase): string {
  switch (phase) {
    case ServerPhase.RUNNING:
      return "running";
    case ServerPhase.STOPPED:
      return "stopped";
    case ServerPhase.PENDING:
      return "pending";
    case ServerPhase.FAILED:
      return "failed";
    default:
      return "unknown";
  }
}

export function Servers() {
  const list = useAsync(() => servers.listServers({}), []);
  const sources = useAsync(() => modSources.listModSources({}), []);
  const action = useAction(list.reload);
  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);
  const [logsFor, setLogsFor] = useState<{ id: string; name: string } | null>(null);

  if (list.loading) return <Spinner label="Loading servers…" />;
  if (list.error) return <Banner kind="error">{list.error}</Banner>;

  const rows = list.data?.servers ?? [];

  return (
    <section>
      <header className="page-header">
        <h2>Servers</h2>
        <Button variant="primary" onClick={() => setCreating((c) => !c)}>
          {creating ? "Cancel" : "New server"}
        </Button>
      </header>

      {action.error && <Banner kind="error">{action.error}</Banner>}

      {creating && (
        <CreateServer
          modSources={(sources.data?.sources ?? []).map((s) => ({
            id: s.id,
            label: s.displayName || s.reference || s.id,
            kind: kindLabel(s.kind),
          }))}
          busy={action.busy}
          onSubmit={(req) =>
            action.run(async () => {
              await servers.createServer(req);
              setCreating(false);
            })
          }
        />
      )}

      {editing && (
        <EditServer
          server={rows.find((s) => s.id === editing)!}
          modSources={(sources.data?.sources ?? []).map((s) => ({
            id: s.id,
            label: s.displayName || s.reference || s.id,
            kind: kindLabel(s.kind),
          }))}
          busy={action.busy}
          onSubmit={(req) =>
            action.run(async () => {
              await servers.updateServer(req);
              setEditing(null);
            })
          }
        />
      )}

      {logsFor && (
        <LogViewer
          serverId={logsFor.id}
          serverName={logsFor.name}
          onClose={() => setLogsFor(null)}
        />
      )}

      <Table
        rows={rows}
        rowKey={(s) => s.id}
        empty="No servers yet."
        columns={[
          { header: "Name", cell: (s) => s.name },
          { header: "Port", cell: (s) => s.port },
          {
            header: "Phase",
            cell: (s) => (
              <span className={`phase phase-${phaseLabel(s.phase)}`}>
                {phaseLabel(s.phase)}
              </span>
            ),
          },
          { header: "Mods", cell: (s) => s.modSourceIds.length },
          // Fetched per row on mount with its own re-check button, as
          // asked -- deliberately not polled: it reads a pod per call,
          // and a fleet page that polls every server would hammer the
          // API server for information that changes slowly.
          { header: "Health", cell: (s) => <ServerHealth serverId={s.id} /> },
          // Only meaningful when the controller has something to say (a
          // failure reason, usually). Takes the leftover width and
          // truncates rather than letting a long bind-conflict message
          // widen the whole table; full text stays in the tooltip.
          {
            header: "Message",
            className: "grow",
            cell: (s) => (
              <span className="muted" title={s.message}>
                {s.message}
              </span>
            ),
          },
          {
            header: "",
            className: "row-actions",
            cell: (s) => (
              <div className="actions">
                {s.desiredState === DesiredState.RUNNING ? (
                  <Button
                    size="compact"
                    disabled={action.busy}
                    onClick={() => action.run(() => servers.stopServer({ id: s.id }))}
                  >
                    Stop
                  </Button>
                ) : (
                  <Button
                    size="compact"
                    disabled={action.busy}
                    onClick={() => action.run(() => servers.startServer({ id: s.id }))}
                  >
                    Start
                  </Button>
                )}
                {/* UpdateServer *is* the force-resync RPC -- it takes
                    only an id and re-pulls every Steam-backed mod source
                    the server references (see its proto doc). Nothing
                    else about a server is editable through it. */}
                <Button
                  size="compact"
                  disabled={action.busy}
                  onClick={() => action.run(() => servers.updateServer({ id: s.id }))}
                >
                  Resync
                </Button>
                <Button size="compact" onClick={() => setLogsFor({ id: s.id, name: s.name })}>
                  Logs
                </Button>
                <Button size="compact" onClick={() => setEditing(editing === s.id ? null : s.id)}>
                  {editing === s.id ? "Cancel" : "Edit"}
                </Button>
                <Button
                  size="compact"
                  variant="danger"
                  disabled={action.busy}
                  onClick={() =>
                    confirmed(`Delete server "${s.name}"? This cannot be undone.`, () =>
                      action.run(() => servers.deleteServer({ id: s.id })),
                    )
                  }
                >
                  Delete
                </Button>
              </div>
            ),
          },
        ]}
      />
    </section>
  );
}

function CreateServer({
  modSources: sources,
  busy,
  onSubmit,
}: {
  modSources: { id: string; label: string; kind: string }[];
  busy: boolean;
  onSubmit: (req: {
    name: string;
    port: number;
    modSourceIds: string[];
    configMap?: string;
    metadata: Record<string, string>;
  }) => void;
}) {
  const [name, setName] = useState("");
  const [port, setPort] = useState(2302);
  const [selected, setSelected] = useState<string[]>([]);
  const [configMap, setConfigMap] = useState("");
  const [metadata, setMetadata] = useState<{ key: string; value: string }[]>([]);

  return (
    <form
      className="card"
      onSubmit={(e) => {
        e.preventDefault();
        onSubmit({
          name: name.trim(),
          port,
          modSourceIds: selected,
          // Empty means "use the baseline" -- sending "" would be a
          // ConfigMap named the empty string.
          configMap: configMap.trim() || undefined,
          metadata: toRecord(metadata),
        });
      }}
    >
      <Field label="Name">
        <input
          value={name}
          required
          onChange={(e) => setName(e.target.value)}
          // Mirrors the DNS-1123 label rule the server enforces
          // (validate_k8s_name in services/gateway) so a bad name is
          // caught here rather than as an RPC error.
          pattern="[a-z0-9]([-a-z0-9]*[a-z0-9])?"
          title="lowercase letters, digits and dashes; must start and end with a letter or digit"
        />
      </Field>
      <Field label="Port">
        <input
          type="number"
          value={port}
          min={1}
          max={65535}
          required
          onChange={(e) => setPort(Number(e.target.value))}
        />
      </Field>
      <Field label="Config override (optional ConfigMap name)">
        <input value={configMap} onChange={(e) => setConfigMap(e.target.value)} />
      </Field>
      <Field label="Mod sources">
        <ModSourcePicker sources={sources} selected={selected} onChange={setSelected} />
      </Field>
      <Field label="Metadata">
        <MetadataEditor entries={metadata} onChange={setMetadata} />
      </Field>
      <Button type="submit" variant="primary" disabled={busy || !name.trim()}>
        {busy ? "Creating…" : "Create"}
      </Button>
    </form>
  );
}

/// Editing an existing server: mod sources and metadata only.
///
/// Name and port aren't editable because UpdateServer cannot change them
/// -- a port move is a delete-and-recreate (the port range is checked for
/// conflicts at creation), and the name is the object's own identity.
function EditServer({
  server,
  modSources: sources,
  busy,
  onSubmit,
}: {
  server: { id: string; name: string; modSourceIds: string[]; metadata: Record<string, string> };
  modSources: { id: string; label: string; kind: string }[];
  busy: boolean;
  onSubmit: (req: {
    id: string;
    modSources: { modSourceIds: string[] };
    metadata: { metadata: Record<string, string> };
  }) => void;
}) {
  const [selected, setSelected] = useState<string[]>(server.modSourceIds);
  const [metadata, setMetadata] = useState(toEntries(server.metadata));

  return (
    <form
      className="card"
      onSubmit={(e) => {
        e.preventDefault();
        onSubmit({
          id: server.id,
          // Both always sent: this form owns both fields, so submitting
          // means "these are the values now". Presence is what tells the
          // server to replace rather than leave alone.
          modSources: { modSourceIds: selected },
          metadata: { metadata: toRecord(metadata) },
        });
      }}
    >
      <h3>Editing {server.name}</h3>
      <p className="muted">
        Saving also forces a resync of every Steam-backed source this server
        references, including any just attached.
      </p>
      <Field label="Mod sources">
        <ModSourcePicker sources={sources} selected={selected} onChange={setSelected} />
      </Field>
      <Field label="Metadata">
        <MetadataEditor entries={metadata} onChange={setMetadata} />
      </Field>
      <Button type="submit" variant="primary" disabled={busy}>
        {busy ? "Saving…" : "Save and resync"}
      </Button>
    </form>
  );
}
