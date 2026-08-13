// Secrets an operator wants referenceable from Arma config ConfigMaps
// via the `secret:` placeholder.
//
// Scoped to the user-secrets namespace only -- the server enforces that,
// and it is the reason that namespace exists separately from the chart's
// own (which holds Postgres credentials and the image pull secret).
//
// Values are never shown, because the server never sends them: ListSecrets
// returns key names only, so a compromised session cannot read secret
// material back out of this page. That makes editing necessarily
// destructive-by-key -- writing a secret replaces its data wholesale, so
// a key left out is removed. The form says so rather than pretending it
// merges.

import { useState } from "react";

import { admin } from "../api/clients";
import { Banner, Button, Field, MetadataEditor, Spinner, Table, confirmed } from "../components/ui";
import { useAction, useAsync } from "../components/useAsync";

export function Secrets() {
  const list = useAsync(() => admin.listSecrets({}), []);
  const action = useAction(list.reload);
  const [editing, setEditing] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);

  if (list.loading) return <Spinner label="Loading secrets…" />;
  if (list.error) {
    return (
      <>
        <Banner kind="error">{list.error}</Banner>
        <p className="muted">
          Managing secrets needs the <code>admin:secrets</code> scope.
        </p>
      </>
    );
  }

  const secrets = list.data?.secrets ?? [];

  return (
    <section>
      <header className="page-header">
        <h2>Secrets</h2>
        <Button variant="primary" onClick={() => setCreating((c) => !c)}>
          {creating ? "Cancel" : "New secret"}
        </Button>
      </header>

      <p className="muted">
        In <code className="mono">{list.data?.namespace}</code>. Reference one from an
        Arma config as <code>secret:&lt;name&gt;/&lt;key&gt;</code>. Values are never
        displayed — only key names are sent to this page.
      </p>

      {action.error && <Banner kind="error">{action.error}</Banner>}

      {creating && (
        <SecretForm
          busy={action.busy}
          onSubmit={(req) =>
            action.run(async () => {
              await admin.putSecret(req);
              setCreating(false);
            })
          }
        />
      )}

      {editing && (
        <SecretForm
          name={editing}
          existingKeys={secrets.find((s) => s.name === editing)?.keys ?? []}
          busy={action.busy}
          onSubmit={(req) =>
            action.run(async () => {
              await admin.putSecret(req);
              setEditing(null);
            })
          }
        />
      )}

      <Table
        rows={secrets}
        rowKey={(s) => s.name}
        empty="No secrets yet."
        columns={[
          { header: "Name", cell: (s) => <span className="mono">{s.name}</span> },
          {
            header: "Keys",
            cell: (s) =>
              s.keys.length === 0 ? (
                <span className="muted">none</span>
              ) : (
                s.keys.map((k) => (
                  <code key={k} className="key-chip">
                    {k}
                  </code>
                ))
              ),
          },
          {
            header: "",
            cell: (s) => (
              <div className="actions">
                <Button onClick={() => setEditing(editing === s.name ? null : s.name)}>
                  {editing === s.name ? "Cancel" : "Replace"}
                </Button>
                <Button
                  variant="danger"
                  disabled={action.busy}
                  onClick={() =>
                    confirmed(
                      `Delete secret "${s.name}"? Any Arma config referencing it will fail to render.`,
                      () => action.run(() => admin.deleteSecret({ name: s.name })),
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

function SecretForm({
  name: fixedName,
  existingKeys,
  busy,
  onSubmit,
}: {
  name?: string;
  existingKeys?: string[];
  busy: boolean;
  onSubmit: (req: { name: string; data: Record<string, string> }) => void;
}) {
  const [name, setName] = useState(fixedName ?? "");
  const [entries, setEntries] = useState<{ key: string; value: string }[]>([]);

  const data: Record<string, string> = {};
  for (const { key, value } of entries) if (key.trim()) data[key.trim()] = value;

  return (
    <form
      className="card"
      onSubmit={(e) => {
        e.preventDefault();
        onSubmit({ name: name.trim(), data });
      }}
    >
      <h3>{fixedName ? `Replace ${fixedName}` : "New secret"}</h3>
      {fixedName && (
        <Banner kind="info">
          This replaces the secret's contents entirely. It currently holds{" "}
          {existingKeys?.length ? (
            existingKeys.map((k) => (
              <code key={k} className="key-chip">
                {k}
              </code>
            ))
          ) : (
            <span>no keys</span>
          )}
          {" — "}any key you don't re-enter below will be removed. Existing values
          cannot be read back, so they must be re-entered.
        </Banner>
      )}
      {!fixedName && (
        <Field label="Name">
          <input
            value={name}
            required
            onChange={(e) => setName(e.target.value)}
            // Mirrors the DNS-1123 rule the server enforces, so a bad
            // name is caught here rather than as an RPC error.
            pattern="[a-z0-9]([-a-z0-9.]*[a-z0-9])?"
            title="lowercase letters, digits, '-' and '.'; must start and end with a letter or digit"
          />
        </Field>
      )}
      <Field label="Keys and values">
        <MetadataEditor entries={entries} onChange={setEntries} />
      </Field>
      <Button
        type="submit"
        variant="primary"
        disabled={busy || !name.trim() || Object.keys(data).length === 0}
      >
        {busy ? "Saving…" : fixedName ? "Replace contents" : "Create"}
      </Button>
    </form>
  );
}
