// Steam Workshop collections published as the cluster's Steam account.
//
// The opposite direction from Mod sources: nothing here syncs anything
// into the cluster. The output is a Steam link to hand to players, whose
// own launchers subscribe to it -- typically built from the same Arma 3
// Launcher preset export the servers run.

import { useState } from "react";

import {
  CollectionVisibility,
  type WorkshopCollectionSummary,
} from "../../../generated/ts/registry/v1/registry_pb";
import { admin, errorMessage } from "../api/clients";
import { Banner, Button, Field, Spinner, Table, confirmed, formatTimestamp } from "../components/ui";
import { useAction, useAsync } from "../components/useAsync";
import {
  type DraftItem,
  VISIBILITY_OPTIONS,
  filterItems,
  membershipDiff,
  mergeItems,
  parseWorkshopRefs,
  titleFromFilename,
  visibilityLabel,
  workshopUrl,
} from "./collectionDraft";

/// What the last save did, shown above the list once the editor closes --
/// the link is the whole point, so it shouldn't vanish with the form.
interface Published {
  title: string;
  url: string;
  count: number;
  created: boolean;
  unresolved: bigint[];
}

type Editing = { mode: "new" } | { mode: "edit"; id: bigint };

export function Collections() {
  const list = useAsync(() => admin.listWorkshopCollections({}), []);
  const action = useAction(list.reload);
  const [editing, setEditing] = useState<Editing | null>(null);
  const [published, setPublished] = useState<Published | null>(null);

  return (
    <section>
      <header className="page-header">
        <h2>Workshop collections</h2>
        <Button
          variant="primary"
          onClick={() => {
            setPublished(null);
            setEditing(editing?.mode === "new" ? null : { mode: "new" });
          }}
        >
          {editing?.mode === "new" ? "Cancel" : "New collection"}
        </Button>
      </header>
      <p className="muted">
        Published on the Steam Workshop as the cluster's Steam account, for players to subscribe
        to from their own launcher. Nothing here is synced to the cluster — add a collection's link
        under Mod sources for that.
      </p>

      {published && <PublishedBanner published={published} />}
      {action.error && <Banner kind="error">{action.error}</Banner>}

      {editing && (
        <CollectionEditor
          // Remounted per target, so switching from one collection's edit
          // to another's (or to "new") never carries a stale draft over.
          key={editing.mode === "edit" ? editing.id.toString() : "new"}
          editId={editing.mode === "edit" ? editing.id : null}
          onCancel={() => setEditing(null)}
          onSaved={(result) => {
            setPublished(result);
            setEditing(null);
            list.reload();
          }}
        />
      )}

      <h3>Published by the cluster</h3>
      {list.loading && <Spinner label="Loading collections…" />}
      {list.error && (
        <Banner kind="error">
          {list.error}
          {/* The one failure with an obvious fix worth spelling out. */}
          {/anonymous|no Steam session/i.test(list.error) &&
            " — sign the cluster into Steam on the Cluster tab first."}
        </Banner>
      )}
      {list.data && (
        <Table<WorkshopCollectionSummary>
          rows={list.data.collections}
          rowKey={(c) => c.id.toString()}
          empty="The cluster's Steam account hasn't published any collections yet."
          columns={[
            {
              header: "Title",
              className: "grow",
              cell: (c) => (
                <a href={c.url} target="_blank" rel="noreferrer noopener">
                  {c.title || c.id.toString()}
                </a>
              ),
            },
            { header: "Visibility", cell: (c) => visibilityLabel(c.visibility) },
            { header: "Mods", cell: (c) => c.itemCount },
            { header: "Updated", cell: (c) => formatTimestamp(c.updatedAtUnixMs) },
            {
              header: "",
              className: "row-actions",
              cell: (c) => (
                <div className="actions">
                  <Button
                    size="compact"
                    onClick={() => {
                      setPublished(null);
                      setEditing({ mode: "edit", id: c.id });
                    }}
                  >
                    Edit
                  </Button>
                  <Button
                    size="compact"
                    variant="danger"
                    disabled={action.busy}
                    onClick={() =>
                      confirmed(
                        `Delete "${c.title}" from the Steam Workshop? Anyone subscribed to it loses the link.`,
                        () =>
                          action.run(async () => {
                            await admin.deleteWorkshopCollection({ collectionId: c.id });
                            if (editing?.mode === "edit" && editing.id === c.id) setEditing(null);
                          }),
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
      )}
    </section>
  );
}

function PublishedBanner({ published }: { published: Published }) {
  return (
    <Banner kind="info">
      {published.created ? "Published" : "Saved"} “{published.title}” with {published.count} mods:{" "}
      <a href={published.url} target="_blank" rel="noreferrer noopener">
        {published.url}
      </a>
      {published.unresolved.length > 0 && (
        <>
          {" "}
          — left out {published.unresolved.length} that Steam wouldn't resolve:{" "}
          <UnresolvedLinks ids={published.unresolved} />
        </>
      )}
    </Banner>
  );
}

function UnresolvedLinks({ ids }: { ids: bigint[] }) {
  return (
    <>
      {ids.map((id, i) => (
        <span key={id.toString()}>
          {i > 0 && ", "}
          <a className="mono" href={workshopUrl(id)} target="_blank" rel="noreferrer noopener">
            {id.toString()}
          </a>
        </span>
      ))}
    </>
  );
}

export function CollectionEditor({
  editId,
  onCancel,
  onSaved,
}: {
  /// null for a new collection.
  editId: bigint | null;
  onCancel: () => void;
  onSaved: (result: Published) => void;
}) {
  // Loading an existing collection is the only async thing that has to
  // happen before the form is usable; a new one starts empty.
  const existing = useAsync(
    () => (editId === null ? Promise.resolve(null) : admin.getWorkshopCollection({ collectionId: editId })),
    [editId],
  );

  if (existing.loading) return <Spinner label="Loading collection…" />;
  if (existing.error) return <Banner kind="error">{existing.error}</Banner>;

  const c = existing.data;
  return (
    <DraftForm
      editId={editId}
      initial={{
        title: c?.title ?? "",
        description: c?.description ?? "",
        // A visibility Steam reports that this build doesn't know comes
        // through as UNSPECIFIED; the select can't show that, and saving
        // has to choose something, so it starts on the safe default.
        visibility:
          c && c.visibility !== CollectionVisibility.UNSPECIFIED
            ? c.visibility
            : CollectionVisibility.UNLISTED,
        items: c?.mods.map((m) => ({ id: m.id, title: m.title })) ?? [],
        unresolved: c?.unresolved ?? [],
      }}
      onCancel={onCancel}
      onSaved={onSaved}
    />
  );
}

function DraftForm({
  editId,
  initial,
  onCancel,
  onSaved,
}: {
  editId: bigint | null;
  initial: {
    title: string;
    description: string;
    visibility: CollectionVisibility;
    items: DraftItem[];
    unresolved: bigint[];
  };
  onCancel: () => void;
  onSaved: (result: Published) => void;
}) {
  const [title, setTitle] = useState(initial.title);
  const [description, setDescription] = useState(initial.description);
  const [visibility, setVisibility] = useState(initial.visibility);
  const [items, setItems] = useState(initial.items);
  // Ids that didn't resolve, from loading the collection or from any
  // import/add since -- shown so a dead link in a preset is noticed.
  const [unresolved, setUnresolved] = useState(initial.unresolved);
  const [refs, setRefs] = useState("");
  const [filter, setFilter] = useState("");
  const [busy, setBusy] = useState<"resolving" | "saving" | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Membership as published, for the "+N / -N" summary when editing.
  const [original] = useState(() => initial.items.map((i) => i.id));

  async function resolveAndAdd(req: { presetHtml?: string; ids?: bigint[] }) {
    setBusy("resolving");
    setError(null);
    try {
      const resolved = await admin.resolveWorkshopItems(req);
      setItems((current) =>
        mergeItems(
          current,
          resolved.mods.map((m) => ({ id: m.id, title: m.title })),
        ),
      );
      setUnresolved((current) => [
        ...current,
        ...resolved.unresolved.filter((id) => !current.includes(id)),
      ]);
      return true;
    } catch (err) {
      setError(errorMessage(err));
      return false;
    } finally {
      setBusy(null);
    }
  }

  async function importPreset(file: File) {
    if (await resolveAndAdd({ presetHtml: await file.text() })) {
      if (!title.trim()) setTitle(titleFromFilename(file.name));
    }
  }

  async function addRefs() {
    const { ids, invalid } = parseWorkshopRefs(refs);
    if (invalid.length > 0) {
      setError(`Not a Workshop link or id: ${invalid.join(", ")}`);
      return;
    }
    if (ids.length === 0) return;
    if (await resolveAndAdd({ ids })) setRefs("");
  }

  async function save(e: React.FormEvent) {
    e.preventDefault();
    setBusy("saving");
    setError(null);
    try {
      const result = await admin.publishWorkshopCollection({
        collectionId: editId ?? 0n,
        title: title.trim(),
        description,
        visibility,
        modIds: items.map((i) => i.id),
      });
      onSaved({
        title: title.trim(),
        url: result.url,
        count: result.mods.length,
        created: result.created,
        unresolved: result.unresolved,
      });
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(null);
    }
  }

  const diff = membershipDiff(original, items);
  const shown = filterItems(items, filter);
  const canSave = busy === null && title.trim() !== "" && items.length > 0;

  return (
    <form className="card" onSubmit={save} aria-label="Collection editor">
      <h3>{editId === null ? "New collection" : `Edit “${initial.title}”`}</h3>
      {error && <Banner kind="error">{error}</Banner>}

      <Field label="Title">
        <input value={title} required onChange={(e) => setTitle(e.target.value)} />
      </Field>
      <Field label="Description">
        <textarea rows={3} value={description} onChange={(e) => setDescription(e.target.value)} />
      </Field>
      <Field label="Visibility">
        <select
          value={visibility}
          onChange={(e) => setVisibility(Number(e.target.value) as CollectionVisibility)}
        >
          {VISIBILITY_OPTIONS.map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
      </Field>

      <Field label="Import an Arma 3 Launcher preset (.html) — adds its mods to the list">
        <input
          type="file"
          accept=".html,.htm,text/html"
          disabled={busy !== null}
          onChange={(e) => {
            const file = e.target.files?.[0];
            // Cleared so picking the same file again (after editing it)
            // still fires a change.
            e.target.value = "";
            if (file) void importPreset(file);
          }}
        />
      </Field>

      <Field label="Add mods or collections — Workshop links or ids, one per line or comma-separated">
        <textarea
          rows={2}
          value={refs}
          disabled={busy !== null}
          placeholder="https://steamcommunity.com/sharedfiles/filedetails/?id=450814997"
          onChange={(e) => setRefs(e.target.value)}
        />
      </Field>
      <div className="actions">
        <Button onClick={() => void addRefs()} disabled={busy !== null || refs.trim() === ""}>
          {busy === "resolving" ? "Looking up…" : "Add"}
        </Button>
      </div>

      {unresolved.length > 0 && (
        <Banner kind="error">
          Steam returned nothing for {unresolved.length} item(s) — removed, private to another
          account, or not a real id. They won't be in the collection:{" "}
          <UnresolvedLinks ids={unresolved} />
        </Banner>
      )}

      <h3>
        Mods ({items.length})
        {editId !== null && (diff.added > 0 || diff.removed > 0) && (
          <span className="muted">
            {" "}
            — {diff.added} added, {diff.removed} removed since it was published
          </span>
        )}
      </h3>
      {items.length > 10 && (
        <Field label="Filter">
          <input value={filter} placeholder="title or id" onChange={(e) => setFilter(e.target.value)} />
        </Field>
      )}
      {/* Scrolls on its own so a 115-mod preset doesn't push the save
          button a page and a half down. */}
      <div className="member-list">
        <Table<DraftItem>
          rows={shown}
          rowKey={(i) => i.id.toString()}
          empty={items.length === 0 ? "No mods yet — import a preset or add some above." : "No mods match."}
          columns={[
            {
              header: "Title",
              className: "grow",
              cell: (i) => (
                <a href={workshopUrl(i.id)} target="_blank" rel="noreferrer noopener">
                  {i.title || "(untitled)"}
                </a>
              ),
            },
            { header: "Workshop id", cell: (i) => <span className="mono">{i.id.toString()}</span> },
            {
              header: "",
              className: "row-actions",
              cell: (i) => (
                <Button
                  size="compact"
                  disabled={busy !== null}
                  onClick={() => setItems((current) => current.filter((x) => x.id !== i.id))}
                >
                  Remove
                </Button>
              ),
            },
          ]}
        />
      </div>
      {items.length > 0 && (
        <div className="actions">
          <Button
            size="compact"
            disabled={busy !== null}
            onClick={() => confirmed(`Remove all ${items.length} mods from the list?`, () => setItems([]))}
          >
            Remove all
          </Button>
        </div>
      )}

      <div className="actions">
        <Button type="submit" variant="primary" disabled={!canSave}>
          {busy === "saving"
            ? "Publishing…"
            : editId === null
              ? "Publish to Steam Workshop"
              : "Save changes"}
        </Button>
        <Button onClick={onCancel} disabled={busy === "saving"}>
          Cancel
        </Button>
      </div>
    </form>
  );
}
