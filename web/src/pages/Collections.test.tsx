import { create } from "@bufbuild/protobuf";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  CollectionVisibility,
  ListWorkshopCollectionsResponseSchema,
  PublishWorkshopCollectionResponseSchema,
  ResolveWorkshopItemsResponseSchema,
  WorkshopCollectionSchema,
  WorkshopCollectionSummarySchema,
  WorkshopItemSchema,
} from "../../../generated/ts/registry/v1/registry_pb";

// The page talks to the cluster only through `admin`; everything else it
// does is local state, which is what these tests exercise.
const admin = vi.hoisted(() => ({
  listWorkshopCollections: vi.fn(),
  getWorkshopCollection: vi.fn(),
  resolveWorkshopItems: vi.fn(),
  publishWorkshopCollection: vi.fn(),
  deleteWorkshopCollection: vi.fn(),
}));
vi.mock("../api/clients", () => ({
  admin,
  errorMessage: (err: unknown) => (err instanceof Error ? err.message : String(err)),
}));

import { Collections } from "./Collections";

// jsdom's File lacks Blob.text(), which every browser the UI targets has
// (and which ModSources' preset upload relies on too). FileReader is the
// part jsdom does implement.
if (!("text" in File.prototype)) {
  Object.defineProperty(Blob.prototype, "text", {
    value(this: Blob) {
      return new Promise<string>((resolve, reject) => {
        const reader = new FileReader();
        reader.onload = () => resolve(reader.result as string);
        reader.onerror = () => reject(reader.error);
        reader.readAsText(this);
      });
    },
  });
}

const item = (id: bigint, title: string) => create(WorkshopItemSchema, { id, title });
const CBA = item(450814997n, "CBA_A3");
const EDEN = item(623475643n, "3den Enhanced");
const ZEN = item(1779063631n, "Zeus Enhanced");

const SUMMARY = create(WorkshopCollectionSummarySchema, {
  id: 3813510055n,
  title: "Bearz Sahatra",
  visibility: CollectionVisibility.UNLISTED,
  itemCount: 115,
  updatedAtUnixMs: 1791133517000n,
  url: "https://steamcommunity.com/sharedfiles/filedetails/?id=3813510055",
});

function listed(...collections: (typeof SUMMARY)[]) {
  return create(ListWorkshopCollectionsResponseSchema, { collections });
}

function resolved(mods: (typeof CBA)[], unresolved: bigint[] = []) {
  return create(ResolveWorkshopItemsResponseSchema, { mods, unresolved });
}

function publishedAs(collectionId: bigint, mods: (typeof CBA)[], created: boolean) {
  return create(PublishWorkshopCollectionResponseSchema, {
    collectionId,
    url: `https://steamcommunity.com/sharedfiles/filedetails/?id=${collectionId}`,
    mods,
    created,
  });
}

function presetFile(name = "BearzSahatra.html") {
  return new File(
    [
      `<a href="https://steamcommunity.com/sharedfiles/filedetails/?id=450814997">CBA</a>
       <a href="https://steamcommunity.com/sharedfiles/filedetails/?id=623475643">3den</a>`,
    ],
    name,
    { type: "text/html" },
  );
}

function editor() {
  return within(screen.getByRole("form", { name: "Collection editor" }));
}

async function openNewEditor() {
  const user = userEvent.setup();
  render(<Collections />);
  await user.click(await screen.findByRole("button", { name: "New collection" }));
  return user;
}

beforeEach(() => {
  admin.listWorkshopCollections.mockResolvedValue(listed(SUMMARY));
  admin.resolveWorkshopItems.mockResolvedValue(resolved([CBA, EDEN]));
  admin.publishWorkshopCollection.mockResolvedValue(publishedAs(42n, [CBA, EDEN], true));
  admin.deleteWorkshopCollection.mockResolvedValue({});
});

describe("collection list", () => {
  it("lists the account's collections with their Steam links", async () => {
    render(<Collections />);
    const link = await screen.findByRole("link", { name: "Bearz Sahatra" });
    expect(link.getAttribute("href")).toBe(SUMMARY.url);
    expect(link.getAttribute("rel")).toContain("noopener");
    const row = link.closest("tr")!;
    expect(within(row).getByText("unlisted")).toBeTruthy();
    expect(within(row).getByText("115")).toBeTruthy();
  });

  it("says so when there are none", async () => {
    admin.listWorkshopCollections.mockResolvedValue(listed());
    render(<Collections />);
    expect(await screen.findByText(/hasn't published any collections/)).toBeTruthy();
  });

  it("points at the Steam login when the cluster has no session", async () => {
    admin.listWorkshopCollections.mockRejectedValue(
      new Error("this needs an authenticated Steam session, but the connection is anonymous"),
    );
    render(<Collections />);
    expect(await screen.findByText(/sign the cluster into Steam on the Cluster tab/)).toBeTruthy();
  });

  it("deletes only after confirmation", async () => {
    const user = userEvent.setup();
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    render(<Collections />);
    const row = (await screen.findByRole("link", { name: "Bearz Sahatra" })).closest("tr")!;

    await user.click(within(row).getByRole("button", { name: "Delete" }));
    expect(confirm).toHaveBeenCalledOnce();
    expect(admin.deleteWorkshopCollection).not.toHaveBeenCalled();

    confirm.mockReturnValue(true);
    await user.click(within(row).getByRole("button", { name: "Delete" }));
    await waitFor(() =>
      expect(admin.deleteWorkshopCollection).toHaveBeenCalledWith({ collectionId: SUMMARY.id }),
    );
    // And the list is refreshed to show it gone.
    await waitFor(() => expect(admin.listWorkshopCollections).toHaveBeenCalledTimes(2));
  });

  it("shows a failed delete without losing the list", async () => {
    const user = userEvent.setup();
    vi.spyOn(window, "confirm").mockReturnValue(true);
    admin.deleteWorkshopCollection.mockRejectedValue(new Error("published by another Steam account"));
    render(<Collections />);
    const row = (await screen.findByRole("link", { name: "Bearz Sahatra" })).closest("tr")!;
    await user.click(within(row).getByRole("button", { name: "Delete" }));
    expect(await screen.findByText("published by another Steam account")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Bearz Sahatra" })).toBeTruthy();
  });
});

describe("new collection from a preset", () => {
  it("imports a preset, titles it after the file, and publishes it unlisted", async () => {
    const user = await openNewEditor();
    const file = presetFile();
    await user.upload(editor().getByLabelText(/Import an Arma 3 Launcher preset/), file);

    expect(await editor().findByRole("link", { name: "CBA_A3" })).toBeTruthy();
    expect(editor().getByRole("link", { name: "3den Enhanced" })).toBeTruthy();
    // The preset goes to the server verbatim; parsing it is server-side.
    expect(admin.resolveWorkshopItems).toHaveBeenCalledWith({ presetHtml: await file.text() });
    expect((editor().getByLabelText("Title") as HTMLInputElement).value).toBe("BearzSahatra");

    await user.click(editor().getByRole("button", { name: "Publish to Steam Workshop" }));

    await waitFor(() => expect(admin.publishWorkshopCollection).toHaveBeenCalledOnce());
    expect(admin.publishWorkshopCollection).toHaveBeenCalledWith({
      collectionId: 0n,
      title: "BearzSahatra",
      description: "",
      visibility: CollectionVisibility.UNLISTED,
      modIds: [CBA.id, EDEN.id],
    });
    // The editor closes and the link stays on screen.
    expect(await screen.findByText(/Published “BearzSahatra” with 2 mods/)).toBeTruthy();
    expect(
      screen.getByRole("link", { name: "https://steamcommunity.com/sharedfiles/filedetails/?id=42" }),
    ).toBeTruthy();
    expect(screen.queryByRole("form", { name: "Collection editor" })).toBeNull();
  });

  it("doesn't overwrite a title already typed", async () => {
    const user = await openNewEditor();
    await user.type(editor().getByLabelText("Title"), "Thursday ops");
    await user.upload(editor().getByLabelText(/Import an Arma 3 Launcher preset/), presetFile());
    await editor().findByRole("link", { name: "CBA_A3" });
    expect((editor().getByLabelText("Title") as HTMLInputElement).value).toBe("Thursday ops");
  });

  it("publishes only what's left after removing a mod", async () => {
    const user = await openNewEditor();
    await user.upload(editor().getByLabelText(/Import an Arma 3 Launcher preset/), presetFile());
    const row = (await editor().findByRole("link", { name: "CBA_A3" })).closest("tr")!;
    await user.click(within(row).getByRole("button", { name: "Remove" }));
    expect(editor().queryByRole("link", { name: "CBA_A3" })).toBeNull();

    await user.click(editor().getByRole("button", { name: "Publish to Steam Workshop" }));
    await waitFor(() =>
      expect(admin.publishWorkshopCollection).toHaveBeenCalledWith(
        expect.objectContaining({ modIds: [EDEN.id] }),
      ),
    );
  });

  it("sends the visibility that was picked", async () => {
    const user = await openNewEditor();
    await user.upload(editor().getByLabelText(/Import an Arma 3 Launcher preset/), presetFile());
    await editor().findByRole("link", { name: "CBA_A3" });
    await user.selectOptions(editor().getByLabelText("Visibility"), "Public");
    await user.click(editor().getByRole("button", { name: "Publish to Steam Workshop" }));
    await waitFor(() =>
      expect(admin.publishWorkshopCollection).toHaveBeenCalledWith(
        expect.objectContaining({ visibility: CollectionVisibility.PUBLIC }),
      ),
    );
  });

  it("can't publish without mods or without a title", async () => {
    const user = await openNewEditor();
    const publish = editor().getByRole("button", { name: "Publish to Steam Workshop" });
    expect(publish).toHaveProperty("disabled", true);

    await user.upload(editor().getByLabelText(/Import an Arma 3 Launcher preset/), presetFile());
    await editor().findByRole("link", { name: "CBA_A3" });
    expect(publish).toHaveProperty("disabled", false);

    await user.clear(editor().getByLabelText("Title"));
    await user.type(editor().getByLabelText("Title"), "   ");
    expect(publish).toHaveProperty("disabled", true);
  });

  it("warns about preset entries Steam couldn't resolve", async () => {
    admin.resolveWorkshopItems.mockResolvedValue(resolved([CBA], [999n]));
    const user = await openNewEditor();
    await user.upload(editor().getByLabelText(/Import an Arma 3 Launcher preset/), presetFile());
    expect(await editor().findByText(/Steam returned nothing for 1 item/)).toBeTruthy();
    expect(editor().getByRole("link", { name: "999" }).getAttribute("href")).toBe(
      "https://steamcommunity.com/sharedfiles/filedetails/?id=999",
    );
  });

  it("shows a rejected preset and keeps the form", async () => {
    admin.resolveWorkshopItems.mockRejectedValue(
      new Error("no Steam Workshop links found -- expected an Arma 3 Launcher preset export"),
    );
    const user = await openNewEditor();
    await user.upload(editor().getByLabelText(/Import an Arma 3 Launcher preset/), presetFile("notes.html"));
    expect(await editor().findByText(/expected an Arma 3 Launcher preset export/)).toBeTruthy();
    // A failed import mustn't name the collection after the wrong file.
    expect((editor().getByLabelText("Title") as HTMLInputElement).value).toBe("");
  });

  it("keeps the draft when publishing fails", async () => {
    admin.publishWorkshopCollection.mockRejectedValue(new Error("failed to publish collection"));
    const user = await openNewEditor();
    await user.upload(editor().getByLabelText(/Import an Arma 3 Launcher preset/), presetFile());
    await editor().findByRole("link", { name: "CBA_A3" });
    await user.click(editor().getByRole("button", { name: "Publish to Steam Workshop" }));
    expect(await editor().findByText("failed to publish collection")).toBeTruthy();
    expect(editor().getByRole("link", { name: "CBA_A3" })).toBeTruthy();
  });
});

describe("adding mods by link", () => {
  it("resolves pasted links and ids, appending only new mods", async () => {
    const user = await openNewEditor();
    await user.upload(editor().getByLabelText(/Import an Arma 3 Launcher preset/), presetFile());
    await editor().findByRole("link", { name: "CBA_A3" });

    // CBA is already listed; only Zeus Enhanced is new.
    admin.resolveWorkshopItems.mockResolvedValue(resolved([CBA, ZEN]));
    await user.type(
      editor().getByLabelText(/Add mods or collections/),
      "https://steamcommunity.com/sharedfiles/filedetails/?id=1779063631, 450814997",
    );
    await user.click(editor().getByRole("button", { name: "Add" }));

    expect(await editor().findByRole("link", { name: "Zeus Enhanced" })).toBeTruthy();
    expect(admin.resolveWorkshopItems).toHaveBeenLastCalledWith({ ids: [ZEN.id, CBA.id] });
    expect(editor().getAllByRole("link", { name: "CBA_A3" })).toHaveLength(1);
    expect(editor().getByText("Mods (3)")).toBeTruthy();
    // Cleared once it worked, ready for the next one.
    expect((editor().getByLabelText(/Add mods or collections/) as HTMLTextAreaElement).value).toBe("");
  });

  it("refuses text that isn't a Workshop link without asking the server", async () => {
    const user = await openNewEditor();
    await user.type(editor().getByLabelText(/Add mods or collections/), "cba_a3");
    await user.click(editor().getByRole("button", { name: "Add" }));
    expect(await editor().findByText("Not a Workshop link or id: cba_a3")).toBeTruthy();
    expect(admin.resolveWorkshopItems).not.toHaveBeenCalled();
  });
});

describe("editing an existing collection", () => {
  beforeEach(() => {
    admin.getWorkshopCollection.mockResolvedValue(
      create(WorkshopCollectionSchema, {
        id: SUMMARY.id,
        title: "Bearz Sahatra",
        description: "Thursday",
        visibility: CollectionVisibility.FRIENDS_ONLY,
        url: SUMMARY.url,
        owned: true,
        mods: [CBA, EDEN],
        unresolved: [777n],
      }),
    );
    admin.publishWorkshopCollection.mockResolvedValue(publishedAs(SUMMARY.id, [EDEN, ZEN], false));
  });

  async function openEdit() {
    const user = userEvent.setup();
    render(<Collections />);
    const row = (await screen.findByRole("link", { name: "Bearz Sahatra" })).closest("tr")!;
    await user.click(within(row).getByRole("button", { name: "Edit" }));
    await screen.findByRole("form", { name: "Collection editor" });
    return user;
  }

  it("loads the collection's current state into the form", async () => {
    await openEdit();
    expect(admin.getWorkshopCollection).toHaveBeenCalledWith({ collectionId: SUMMARY.id });
    expect((editor().getByLabelText("Title") as HTMLInputElement).value).toBe("Bearz Sahatra");
    expect((editor().getByLabelText("Description") as HTMLTextAreaElement).value).toBe("Thursday");
    expect((editor().getByLabelText("Visibility") as HTMLSelectElement).value).toBe(
      String(CollectionVisibility.FRIENDS_ONLY),
    );
    expect(editor().getByRole("link", { name: "CBA_A3" })).toBeTruthy();
    // Dead members are called out, since saving will drop them.
    expect(editor().getByText(/Steam returned nothing for 1 item/)).toBeTruthy();
  });

  it("summarises membership changes and saves them in place", async () => {
    const user = await openEdit();
    const row = editor().getByRole("link", { name: "CBA_A3" }).closest("tr")!;
    await user.click(within(row).getByRole("button", { name: "Remove" }));
    admin.resolveWorkshopItems.mockResolvedValue(resolved([ZEN]));
    await user.type(editor().getByLabelText(/Add mods or collections/), "1779063631");
    await user.click(editor().getByRole("button", { name: "Add" }));
    expect(await editor().findByText(/1 added, 1 removed since it was published/)).toBeTruthy();

    await user.click(editor().getByRole("button", { name: "Save changes" }));
    await waitFor(() =>
      expect(admin.publishWorkshopCollection).toHaveBeenCalledWith({
        collectionId: SUMMARY.id,
        title: "Bearz Sahatra",
        description: "Thursday",
        visibility: CollectionVisibility.FRIENDS_ONLY,
        modIds: [EDEN.id, ZEN.id],
      }),
    );
    expect(await screen.findByText(/Saved “Bearz Sahatra” with 2 mods/)).toBeTruthy();
  });

  it("falls back to Unlisted for a visibility it can't show", async () => {
    // A Steam visibility this build doesn't know arrives as UNSPECIFIED,
    // which the server would refuse on save.
    admin.getWorkshopCollection.mockResolvedValue(
      create(WorkshopCollectionSchema, {
        id: SUMMARY.id,
        title: "Bearz Sahatra",
        owned: true,
        mods: [CBA],
      }),
    );
    await openEdit();
    expect((editor().getByLabelText("Visibility") as HTMLSelectElement).value).toBe(
      String(CollectionVisibility.UNLISTED),
    );
  });

  it("shows a collection that failed to load instead of an empty form", async () => {
    admin.getWorkshopCollection.mockRejectedValue(new Error("isn't visible to the cluster's Steam account"));
    const user = userEvent.setup();
    render(<Collections />);
    const row = (await screen.findByRole("link", { name: "Bearz Sahatra" })).closest("tr")!;
    await user.click(within(row).getByRole("button", { name: "Edit" }));
    expect(await screen.findByText(/isn't visible to the cluster's Steam account/)).toBeTruthy();
    expect(screen.queryByRole("form", { name: "Collection editor" })).toBeNull();
  });

  it("filters a long list without changing what gets saved", async () => {
    const many = Array.from({ length: 12 }, (_, i) => item(BigInt(1000 + i), `Mod ${i}`));
    admin.getWorkshopCollection.mockResolvedValue(
      create(WorkshopCollectionSchema, {
        id: SUMMARY.id,
        title: "Big",
        visibility: CollectionVisibility.UNLISTED,
        owned: true,
        mods: many,
      }),
    );
    const user = await openEdit();
    await user.type(editor().getByLabelText("Filter"), "Mod 11");
    expect(editor().getAllByRole("link")).toHaveLength(1);

    await user.click(editor().getByRole("button", { name: "Save changes" }));
    await waitFor(() =>
      expect(admin.publishWorkshopCollection).toHaveBeenCalledWith(
        expect.objectContaining({ modIds: many.map((m) => m.id) }),
      ),
    );
  });
});
