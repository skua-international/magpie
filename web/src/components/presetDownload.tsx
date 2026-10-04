// "Download as preset": one or more mod sources rendered server-side as
// an Arma 3 Launcher preset export (ModSourceService.ExportPreset), handed
// to the browser as a file. Shared by the mod source rows (one source)
// and server rows (every source the server loads).

import { useCallback, useState } from "react";

import type { ExportPresetResponse } from "../../../generated/ts/registry/v1/registry_pb";
import { errorMessage, modSources } from "../api/clients";
import { Banner } from "./ui";

/// A filename for the preset, from its name. The Launcher names imported
/// presets from the file's own metadata, not its filename, so this only
/// has to be a safe, recognisable name on disk.
export function presetFilename(name: string): string {
  const safe = name
    .trim()
    .replace(/[^\w\- ]+/g, "")
    .replace(/\s+/g, "_");
  return `${safe || "preset"}.html`;
}

/// What was left out of an export, in a sentence -- or null when the
/// preset holds everything that was asked for.
export function exportNotes(resp: ExportPresetResponse): string | null {
  const notes: string[] = [];
  if (resp.skippedLocalSources.length > 0)
    notes.push(
      `left out local mods, which players can't get from the Workshop (${resp.skippedLocalSources.join(", ")})`,
    );
  if (resp.unresolvedSources.length > 0)
    notes.push(`left out sources not resolved from Steam yet (${resp.unresolvedSources.join(", ")})`);
  if (resp.missingSources.length > 0)
    notes.push(`skipped sources that no longer exist (${resp.missingSources.join(", ")})`);
  return notes.length > 0 ? `${notes.join("; ")}.` : null;
}

/// Hand `html` to the browser as a download. An object URL rather than a
/// data: URL, which some browsers cap in length -- a 115-mod preset is
/// ~60 KB.
export function saveFile(html: string, filename: string) {
  const url = URL.createObjectURL(new Blob([html], { type: "text/html" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

export function usePresetDownload() {
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<{ kind: "error" | "info"; text: string } | null>(null);

  const download = useCallback(async (sourceIds: string[], name: string) => {
    setBusy(true);
    setNotice(null);
    try {
      const resp = await modSources.exportPreset({ sourceIds, name });
      saveFile(resp.html, presetFilename(name));
      const notes = exportNotes(resp);
      setNotice({
        kind: "info",
        text: `Downloaded “${name}” as a Launcher preset with ${resp.modCount} mods${notes ? ` — ${notes}` : "."}`,
      });
    } catch (err) {
      setNotice({ kind: "error", text: errorMessage(err) });
    } finally {
      setBusy(false);
    }
  }, []);

  const banner = notice && <Banner kind={notice.kind}>{notice.text}</Banner>;
  return { download, busy, banner };
}
