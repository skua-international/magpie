// Mission (.pbo) storage: list, upload, delete.

import { useState } from "react";

import { missions } from "../api/clients";
import {
  Banner,
  Button,
  Field,
  MetadataChips,
  MetadataEditor,
  MetadataForm,
  toRecord,
  Spinner,
  Table,
  confirmed,
  formatBytes,
  formatTimestamp,
} from "../components/ui";
import { useAction, useAsync } from "../components/useAsync";

export function Missions() {
  const list = useAsync(() => missions.listMissions({}), []);
  const action = useAction(list.reload);
  const [file, setFile] = useState<File | null>(null);
  const [name, setName] = useState("");
  const [metadata, setMetadata] = useState<{ key: string; value: string }[]>([]);
  const [editing, setEditing] = useState<string | null>(null);

  if (list.loading) return <Spinner label="Loading missions…" />;
  if (list.error) return <Banner kind="error">{list.error}</Banner>;

  return (
    <section>
      <header className="page-header">
        <h2>Missions</h2>
      </header>

      {action.error && <Banner kind="error">{action.error}</Banner>}

      <form
        className="card"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!file) return;
          const bytes = new Uint8Array(await file.arrayBuffer());
          await action.run(async () => {
            await missions.uploadMission({
              // Defaults to the file's own name, which is nearly always
              // what's wanted (the .pbo is named after the mission).
              name: name.trim() || file.name,
              pboContent: bytes,
              metadata: toRecord(metadata),
            });
            setFile(null);
            setName("");
            setMetadata([]);
          });
        }}
      >
        <Field label="Mission .pbo">
          <input
            type="file"
            accept=".pbo"
            required
            onChange={(e) => setFile(e.target.files?.[0] ?? null)}
          />
        </Field>
        <Field label="Name (defaults to the filename)">
          <input value={name} onChange={(e) => setName(e.target.value)} />
        </Field>
        <Field label="Metadata">
          <MetadataEditor entries={metadata} onChange={setMetadata} />
        </Field>
        <Button type="submit" variant="primary" disabled={action.busy || !file}>
          {action.busy ? "Uploading…" : "Upload"}
        </Button>
      </form>

      {editing && (
        <MetadataForm
          title={`Metadata — ${
            list.data?.missions.find((m) => m.id === editing)?.name || editing
          }`}
          initial={list.data?.missions.find((m) => m.id === editing)?.metadata ?? {}}
          busy={action.busy}
          onCancel={() => setEditing(null)}
          onSubmit={(metadata) =>
            action.run(async () => {
              await missions.setMissionMetadata({ id: editing, metadata });
              setEditing(null);
            })
          }
        />
      )}

      <Table
        rows={list.data?.missions ?? []}
        rowKey={(m) => m.id}
        empty="No missions uploaded."
        columns={[
          { header: "Name", cell: (m) => m.name },
          { header: "Size", cell: (m) => formatBytes(m.filesize) },
          { header: "Uploaded", cell: (m) => formatTimestamp(m.createdAtUnixMs) },
          {
            header: "By",
            className: "grow",
            cell: (m) => <span className="muted">{m.createdBy || "—"}</span>,
          },
          { header: "Metadata", cell: (m) => <MetadataChips metadata={m.metadata} /> },
          {
            header: "",
            className: "row-actions",
            cell: (m) => (
              <div className="actions">
              <Button
                size="compact"
                onClick={() => setEditing(editing === m.id ? null : m.id)}
              >
                {editing === m.id ? "Cancel" : "Metadata"}
              </Button>
              <Button
                size="compact"
                variant="danger"
                disabled={action.busy}
                onClick={() =>
                  confirmed(`Delete mission "${m.name}"?`, () =>
                    action.run(() => missions.deleteMission({ id: m.id })),
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
