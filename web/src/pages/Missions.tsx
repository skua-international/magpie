// Mission (.pbo) storage: list, upload, delete.

import { useState } from "react";

import { missions } from "../api/clients";
import {
  Banner,
  Button,
  Field,
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
            });
            setFile(null);
            setName("");
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
        <Button type="submit" variant="primary" disabled={action.busy || !file}>
          {action.busy ? "Uploading…" : "Upload"}
        </Button>
      </form>

      <Table
        rows={list.data?.missions ?? []}
        rowKey={(m) => m.id}
        empty="No missions uploaded."
        columns={[
          { header: "Name", cell: (m) => m.name },
          { header: "Size", cell: (m) => formatBytes(m.filesize) },
          { header: "Uploaded", cell: (m) => formatTimestamp(m.createdAtUnixMs) },
          { header: "By", cell: (m) => <span className="muted">{m.createdBy || "—"}</span> },
          {
            header: "",
            className: "row-actions",
            cell: (m) => (
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
            ),
          },
        ]}
      />
    </section>
  );
}
