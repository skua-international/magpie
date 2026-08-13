// Mod sources: the three registration shapes AddModSource's `source`
// oneof accepts (Steam URL, preset HTML by URL or pasted content, local
// zip upload), plus sync and delete.

import { useState } from "react";

import { ModSourceKind } from "../../../generated/ts/registry/v1/registry_pb";
import { modSources } from "../api/clients";
import {
  Banner,
  Button,
  Field,
  Reference,
  Spinner,
  Table,
  confirmed,
  formatBytes,
  formatTimestamp,
} from "../components/ui";
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

type Shape = "steam" | "presetUrl" | "presetContent" | "local";

export function ModSources() {
  const list = useAsync(() => modSources.listModSources({}), []);
  const action = useAction(list.reload);
  const [adding, setAdding] = useState(false);

  if (list.loading) return <Spinner label="Loading mod sources…" />;
  if (list.error) return <Banner kind="error">{list.error}</Banner>;

  return (
    <section>
      <header className="page-header">
        <h2>Mod sources</h2>
        <Button variant="primary" onClick={() => setAdding((a) => !a)}>
          {adding ? "Cancel" : "Add source"}
        </Button>
      </header>

      {action.error && <Banner kind="error">{action.error}</Banner>}

      {adding && (
        <AddSource
          busy={action.busy}
          onSubmit={(req) =>
            action.run(async () => {
              await modSources.addModSource(req);
              setAdding(false);
            })
          }
        />
      )}

      <Table
        rows={list.data?.sources ?? []}
        rowKey={(s) => s.id}
        empty="No mod sources registered."
        columns={[
          { header: "Name", cell: (s) => s.displayName || "—" },
          { header: "Kind", cell: (s) => kindLabel(s.kind) },
          { header: "Reference", cell: (s) => <Reference reference={s.reference} /> },
          { header: "Size", cell: (s) => formatBytes(s.sizeBytes) },
          { header: "Added", cell: (s) => formatTimestamp(s.createdAtUnixMs) },
          {
            header: "",
            cell: (s) => (
              <div className="actions">
                <Button
                  disabled={action.busy}
                  onClick={() => action.run(() => modSources.syncModSource({ id: s.id }))}
                >
                  Sync
                </Button>
                <Button
                  variant="danger"
                  disabled={action.busy}
                  onClick={() =>
                    confirmed(
                      `Delete mod source "${s.displayName || s.reference}"?`,
                      () => action.run(() => modSources.deleteModSource({ id: s.id })),
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

function AddSource({
  busy,
  onSubmit,
}: {
  busy: boolean;
  // The generated request type is a oneof; each branch below builds
  // exactly one case rather than sending a half-filled object.
  onSubmit: (req: Parameters<typeof modSources.addModSource>[0]) => void;
}) {
  const [shape, setShape] = useState<Shape>("steam");
  const [text, setText] = useState("");
  const [uniqueId, setUniqueId] = useState("");
  const [file, setFile] = useState<File | null>(null);
  const [presetFile, setPresetFile] = useState<File | null>(null);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    switch (shape) {
      case "steam":
        return onSubmit({ source: { case: "steamUrl", value: text.trim() } });
      case "presetUrl":
        return onSubmit({ source: { case: "htmlUrl", value: text.trim() } });
      case "presetContent": {
        // File-picked and read as text, same shape as the zip upload --
        // a preset export is a file people already have on disk, so
        // picking it beats opening it in an editor to copy its markup.
        // The RPC still takes the content itself (html_content), so this
        // is purely how the content is obtained.
        if (!presetFile) return;
        return onSubmit({
          source: { case: "htmlContent", value: await presetFile.text() },
        });
      }
      case "local": {
        if (!file) return;
        // A plain <input type=file> plus ArrayBuffer is the whole story
        // for zip upload -- no native app capability needed, which is
        // why this stayed a browser UI.
        const bytes = new Uint8Array(await file.arrayBuffer());
        return onSubmit({
          source: {
            case: "localMod",
            value: { uniqueId: uniqueId.trim(), zipContent: bytes },
          },
        });
      }
    }
  }

  return (
    <form className="card" onSubmit={submit}>
      <Field label="Source type">
        <select value={shape} onChange={(e) => setShape(e.target.value as Shape)}>
          <option value="steam">Steam Workshop URL (mod or collection)</option>
          <option value="presetUrl">Preset HTML by URL</option>
          <option value="presetContent">Preset HTML (upload file)</option>
          <option value="local">Local mod (.zip upload)</option>
        </select>
      </Field>

      {shape === "presetContent" && (
        <Field label="Preset HTML file">
          <input
            type="file"
            accept=".html,.htm,text/html"
            required
            onChange={(e) => setPresetFile(e.target.files?.[0] ?? null)}
          />
        </Field>
      )}

      {(shape === "steam" || shape === "presetUrl") && (
        <Field label="URL">
          <input
            type="url"
            value={text}
            required
            placeholder={
              shape === "steam"
                ? "https://steamcommunity.com/sharedfiles/filedetails/?id=…"
                : "https://…/preset.html"
            }
            onChange={(e) => setText(e.target.value)}
          />
        </Field>
      )}

      {shape === "local" && (
        <>
          <Field label="Unique ID (its on-disk name and stable reference)">
            <input
              value={uniqueId}
              required
              onChange={(e) => setUniqueId(e.target.value)}
            />
          </Field>
          <Field label="Zip archive">
            <input
              type="file"
              accept=".zip,application/zip"
              required
              onChange={(e) => setFile(e.target.files?.[0] ?? null)}
            />
          </Field>
        </>
      )}

      <Button type="submit" variant="primary" disabled={busy}>
        {busy ? "Adding…" : "Add"}
      </Button>
    </form>
  );
}
