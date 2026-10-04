import { create } from "@bufbuild/protobuf";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  ListServersResponseSchema,
  ServerInfoSchema,
} from "../../../generated/ts/controller/v1/controller_pb";
import {
  ExportPresetResponseSchema,
  ListModSourcesResponseSchema,
  ModSourceInfoSchema,
  ModSourceKind,
} from "../../../generated/ts/registry/v1/registry_pb";

const clients = vi.hoisted(() => ({
  modSources: {
    exportPreset: vi.fn(),
    listModSources: vi.fn(),
  },
  servers: {
    listServers: vi.fn(),
    getServerHealth: vi.fn(),
  },
}));
vi.mock("../api/clients", () => ({
  ...clients,
  errorMessage: (err: unknown) => (err instanceof Error ? err.message : String(err)),
}));

import { ModSources } from "../pages/ModSources";
import { Servers } from "../pages/Servers";
import { exportNotes, presetFilename, saveFile } from "./presetDownload";

const exported = (
  over: {
    modCount?: number;
    skippedLocalSources?: string[];
    unresolvedSources?: string[];
    missingSources?: string[];
  } = {},
) =>
  create(ExportPresetResponseSchema, { html: "<html>preset</html>", modCount: 115, ...over });

// jsdom implements neither object URLs nor navigation, so a download is
// observed as: a blob URL was made for the HTML, and an <a download> was
// clicked with the right filename.
const downloads: { filename: string; blob: Blob }[] = [];
beforeEach(() => {
  downloads.length = 0;
  let blob: Blob | null = null;
  URL.createObjectURL = vi.fn((b: Blob) => {
    blob = b;
    return "blob:preset";
  });
  URL.revokeObjectURL = vi.fn();
  vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) {
    downloads.push({ filename: this.download, blob: blob! });
  });
  clients.modSources.exportPreset.mockResolvedValue(exported());
  clients.servers.getServerHealth.mockReturnValue(new Promise(() => {}));
});

describe("presetFilename", () => {
  it("keeps a readable name and adds the extension", () => {
    expect(presetFilename("Bearz Sahatra")).toBe("Bearz_Sahatra.html");
    expect(presetFilename("ops-2026")).toBe("ops-2026.html");
  });

  it("strips anything a filesystem might object to", () => {
    expect(presetFilename("../../etc/passwd")).toBe("etcpasswd.html");
    expect(presetFilename('Sa\'hatra: "ops" <1>')).toBe("Sahatra_ops_1.html");
  });

  it("never produces an empty name", () => {
    expect(presetFilename("")).toBe("preset.html");
    expect(presetFilename("???")).toBe("preset.html");
  });
});

describe("exportNotes", () => {
  it("is null when nothing was left out", () => {
    expect(exportNotes(exported())).toBeNull();
  });

  it("names every kind of omission", () => {
    const notes = exportNotes(
      exported({
        skippedLocalSources: ["skua_custom"],
        unresolvedSources: ["pending"],
        missingSources: ["gone"],
      }),
    )!;
    expect(notes).toContain("local mods");
    expect(notes).toContain("skua_custom");
    expect(notes).toContain("not resolved from Steam yet (pending)");
    expect(notes).toContain("no longer exist (gone)");
  });
});

describe("saveFile", () => {
  it("downloads the HTML under the given name and frees the URL", async () => {
    saveFile("<html>x</html>", "x.html");
    expect(downloads).toHaveLength(1);
    expect(downloads[0].filename).toBe("x.html");
    expect(downloads[0].blob.type).toBe("text/html");
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:preset");
    // Nothing left behind in the page.
    expect(document.querySelector("a[download]")).toBeNull();
  });
});

describe("mod source rows", () => {
  beforeEach(() => {
    clients.modSources.listModSources.mockResolvedValue(
      create(ListModSourcesResponseSchema, {
        sources: [
          create(ModSourceInfoSchema, {
            id: "sahatra",
            kind: ModSourceKind.COLLECTION,
            displayName: "Bearz Sahatra",
            reference: "https://steamcommunity.com/sharedfiles/filedetails/?id=1",
          }),
          create(ModSourceInfoSchema, { id: "skua_custom", kind: ModSourceKind.LOCAL, reference: "skua_custom" }),
        ],
      }),
    );
  });

  it("export a collection source as a preset named after it", async () => {
    const user = userEvent.setup();
    render(<ModSources />);
    const row = (await screen.findByText("Bearz Sahatra")).closest("tr")!;
    await user.click([...row.querySelectorAll("button")].find((b) => b.textContent === "Preset")!);

    await waitFor(() => expect(downloads).toHaveLength(1));
    expect(clients.modSources.exportPreset).toHaveBeenCalledWith({
      sourceIds: ["sahatra"],
      name: "Bearz Sahatra",
    });
    expect(downloads[0].filename).toBe("Bearz_Sahatra.html");
    expect(await downloads[0].blob.text()).toBe("<html>preset</html>");
    expect(await screen.findByText(/Downloaded “Bearz Sahatra” as a Launcher preset with 115 mods/)).toBeTruthy();
  });

  it("offer no preset for a local source", async () => {
    render(<ModSources />);
    const row = (await screen.findByText("skua_custom", { selector: "span" })).closest("tr")!;
    expect([...row.querySelectorAll("button")].map((b) => b.textContent)).not.toContain("Preset");
  });

  it("show a refused export instead of downloading", async () => {
    clients.modSources.exportPreset.mockRejectedValue(new Error("nothing to export: not resolved from Steam yet (sahatra)"));
    const user = userEvent.setup();
    render(<ModSources />);
    const row = (await screen.findByText("Bearz Sahatra")).closest("tr")!;
    await user.click([...row.querySelectorAll("button")].find((b) => b.textContent === "Preset")!);
    expect(await screen.findByText(/nothing to export/)).toBeTruthy();
    expect(downloads).toHaveLength(0);
  });
});

describe("server rows", () => {
  beforeEach(() => {
    clients.modSources.listModSources.mockResolvedValue(create(ListModSourcesResponseSchema, {}));
    clients.servers.listServers.mockResolvedValue(
      create(ListServersResponseSchema, {
        servers: [
          create(ServerInfoSchema, { id: "s1", name: "Thursday Ops", modSourceIds: ["sahatra", "cba", "skua_custom"] }),
          create(ServerInfoSchema, { id: "s2", name: "Vanilla", modSourceIds: [] }),
        ],
      }),
    );
  });

  function presetButton(name: string) {
    const row = screen.getByText(name).closest("tr")!;
    return [...row.querySelectorAll("button")].find((b) => b.textContent === "Preset")!;
  }

  it("export every source the server loads as one preset, and say what was left out", async () => {
    clients.modSources.exportPreset.mockResolvedValue(exported({ modCount: 120, skippedLocalSources: ["skua_custom"] }));
    const user = userEvent.setup();
    render(<Servers />);
    await screen.findByText("Thursday Ops");
    await user.click(presetButton("Thursday Ops"));

    await waitFor(() => expect(downloads).toHaveLength(1));
    expect(clients.modSources.exportPreset).toHaveBeenCalledWith({
      sourceIds: ["sahatra", "cba", "skua_custom"],
      name: "Thursday Ops",
    });
    expect(downloads[0].filename).toBe("Thursday_Ops.html");
    expect(await screen.findByText(/with 120 mods — left out local mods.*skua_custom/)).toBeTruthy();
  });

  it("disable it for a server with no mod sources", async () => {
    render(<Servers />);
    await screen.findByText("Vanilla");
    expect(presetButton("Vanilla")).toHaveProperty("disabled", true);
  });
});
