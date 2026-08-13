// Shared primitives. Deliberately hand-rolled rather than pulling in a
// component library: this is an admin dashboard with tables, buttons and
// a couple of forms, and a dependency whose surface dwarfs the app is a
// long-term maintenance cost for contributors to learn on top of React
// itself.

import type { ReactNode } from "react";

export function Button({
  children,
  onClick,
  disabled,
  variant = "default",
  type = "button",
  size = "default",
}: {
  children: ReactNode;
  onClick?: () => void;
  disabled?: boolean;
  variant?: "default" | "primary" | "danger";
  type?: "button" | "submit";
  /// "compact" is for buttons living inside table rows, where full-size
  /// padding is what forces a row tall enough to need scrolling past.
  size?: "default" | "compact";
}) {
  return (
    <button
      type={type}
      className={`btn btn-${variant}${size === "compact" ? " btn-compact" : ""}`}
      onClick={onClick}
      disabled={disabled}
    >
      {children}
    </button>
  );
}

/// Every destructive action goes through this rather than a bare click.
/// window.confirm is intentional -- it cannot be missed, cannot be styled
/// into something ignorable, and needs no modal state machine.
export function confirmed(message: string, action: () => void) {
  if (window.confirm(message)) action();
}

export function Banner({ kind, children }: { kind: "error" | "info"; children: ReactNode }) {
  return <div className={`banner banner-${kind}`}>{children}</div>;
}

export function Spinner({ label }: { label: string }) {
  return <p className="muted">{label}</p>;
}

/// Renders a table, or an explicit empty-state line instead of an empty
/// table -- "no servers yet" reads as a working page, whereas headers
/// over nothing reads as a failed load.
export function Table<T>({
  rows,
  columns,
  empty,
  rowKey,
}: {
  rows: T[];
  columns: { header: string; cell: (row: T) => ReactNode; className?: string }[];
  empty: string;
  rowKey: (row: T) => string;
}) {
  if (rows.length === 0) return <p className="muted">{empty}</p>;
  return (
    // Scrolling lives on this wrapper, not on the table itself. A table
    // with `display: block` (which is what makes overflow work directly
    // on it) stops honouring width:100% and stops distributing column
    // widths, which is what pushed the actions column off-screen.
    <div className="table-scroll">
      <table>
      <thead>
        <tr>
          {columns.map((c) => (
            <th key={c.header} className={c.className}>
              {c.header}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.map((row) => (
          <tr key={rowKey(row)}>
            {columns.map((c) => (
              <td key={c.header} className={c.className}>
                {c.cell(row)}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
      </table>
    </div>
  );
}

export function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="field">
      <span>{label}</span>
      {children}
    </label>
  );
}

export function formatBytes(bytes: bigint | number): string {
  const n = typeof bytes === "bigint" ? Number(bytes) : bytes;
  if (n === 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  const i = Math.min(Math.floor(Math.log(n) / Math.log(1024)), units.length - 1);
  return `${(n / 1024 ** i).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

export function formatTimestamp(unixMs: bigint | number): string {
  const n = typeof unixMs === "bigint" ? Number(unixMs) : unixMs;
  if (!n) return "—";
  return new Date(n).toLocaleString();
}

/// Renders a mod source's `reference` as a link when it is one.
///
/// Steam and preset-URL sources carry a real URL; local sources carry a
/// caller-assigned id and preset-from-content sources carry the literal
/// "(uploaded HTML)", neither of which is navigable. Checking the parsed
/// protocol rather than a "http" prefix keeps a javascript: or data:
/// reference from ever becoming a clickable link.
export function Reference({ reference }: { reference: string }) {
  let href: string | null = null;
  try {
    const url = new URL(reference);
    if (url.protocol === "http:" || url.protocol === "https:") href = url.href;
  } catch {
    href = null;
  }
  if (!href) return <span className="mono">{reference}</span>;
  return (
    <a className="mono" href={href} target="_blank" rel="noreferrer noopener">
      {reference}
    </a>
  );
}

/// Checkbox list for picking mod sources. Replaces a multi-select, which
/// hides its selection behind ctrl-click and silently loses it on a
/// stray click.
export function ModSourcePicker({
  sources,
  selected,
  onChange,
}: {
  sources: { id: string; label: string; kind: string }[];
  selected: string[];
  onChange: (ids: string[]) => void;
}) {
  if (sources.length === 0) return <p className="muted">No mod sources registered yet.</p>;
  return (
    <div className="picker">
      {sources.map((s) => (
        <label key={s.id}>
          <input
            type="checkbox"
            checked={selected.includes(s.id)}
            onChange={() =>
              onChange(
                selected.includes(s.id)
                  ? selected.filter((id) => id !== s.id)
                  : [...selected, s.id],
              )
            }
          />
          <span>{s.label}</span>
          <span className="muted kind">{s.kind}</span>
        </label>
      ))}
    </div>
  );
}

/// Key/value editor: a row per entry plus an "Add" button, as asked for.
/// Rows are addressed by index rather than by key so that editing a key
/// doesn't remount the row and lose focus after every keystroke.
export function MetadataEditor({
  entries,
  onChange,
}: {
  entries: { key: string; value: string }[];
  onChange: (entries: { key: string; value: string }[]) => void;
}) {
  const update = (i: number, patch: Partial<{ key: string; value: string }>) =>
    onChange(entries.map((e, j) => (i === j ? { ...e, ...patch } : e)));

  return (
    <div className="metadata">
      {entries.map((entry, i) => (
        <div className="metadata-row" key={i}>
          <input
            placeholder="key"
            value={entry.key}
            onChange={(e) => update(i, { key: e.target.value })}
          />
          <input
            placeholder="value"
            value={entry.value}
            onChange={(e) => update(i, { value: e.target.value })}
          />
          <Button onClick={() => onChange(entries.filter((_, j) => j !== i))}>
            Remove
          </Button>
        </div>
      ))}
      <Button onClick={() => onChange([...entries, { key: "", value: "" }])}>
        Add metadata
      </Button>
    </div>
  );
}
