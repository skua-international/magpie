// ACL management -- the one thing here with no magpiectl equivalent, and
// the reason a form-based UI earns its place: assigning scopes is a
// per-person checkbox matrix, which reads far better than flags.
//
// Backed by AdminService.ListAcl/SetAclScopes, added alongside this page
// (the grant table was previously only readable one subject at a time by
// the authorization middleware, and only writable on first login).

import { useState } from "react";

import type { AclSubject } from "../../../generated/ts/registry/v1/registry_pb";
import { admin } from "../api/clients";
import { Banner, Button, Spinner, confirmed } from "../components/ui";
import { useAction, useAsync } from "../components/useAsync";

/// How a person is identified in the list. The subject is an opaque user
/// id, so linked accounts are what anyone actually recognizes; the id is
/// the fallback for a user who somehow has none.
function displayName(subject: AclSubject): string {
  const named = subject.accounts.find((a) => a.displayName);
  if (named) return `${named.displayName} (${named.provider})`;
  const any = subject.accounts[0];
  return any ? `${any.provider}:${any.providerUserId}` : subject.subject;
}

export function Access() {
  const list = useAsync(() => admin.listAcl({}), []);
  const action = useAction(list.reload);

  if (list.loading) return <Spinner label="Loading access…" />;
  if (list.error) {
    return (
      <>
        <Banner kind="error">{list.error}</Banner>
        <p className="muted">
          Managing access needs the <code>admin:acl</code> scope, which is separate
          from every other admin scope because it can grant all of them.
        </p>
      </>
    );
  }

  const subjects = list.data?.subjects ?? [];
  const knownScopes = list.data?.knownScopes ?? [];

  return (
    <section>
      <header className="page-header">
        <h2>Access</h2>
      </header>

      <p className="muted">
        A user appears here once they have signed in at least once. <code>*</code>{" "}
        grants every scope, present and future.
      </p>

      {action.error && <Banner kind="error">{action.error}</Banner>}

      {subjects.length === 0 ? (
        <p className="muted">Nobody has signed in yet.</p>
      ) : (
        subjects.map((subject) => (
          <SubjectCard
            key={subject.subject}
            subject={subject}
            knownScopes={knownScopes}
            busy={action.busy}
            onSave={(scopes) =>
              action.run(() =>
                admin.setAclScopes({ subject: subject.subject, scopes }),
              )
            }
          />
        ))
      )}
    </section>
  );
}

function SubjectCard({
  subject,
  knownScopes,
  busy,
  onSave,
}: {
  subject: AclSubject;
  knownScopes: string[];
  busy: boolean;
  onSave: (scopes: string[]) => void;
}) {
  // Local edit buffer so checkboxes respond immediately and only a
  // deliberate Save writes -- the RPC replaces the whole set, so a
  // per-checkbox write would be a stream of full overwrites.
  const [scopes, setScopes] = useState<string[]>(subject.scopes);
  const dirty =
    scopes.length !== subject.scopes.length ||
    scopes.some((s) => !subject.scopes.includes(s));
  const isAdmin = scopes.includes("*");

  function toggle(scope: string) {
    setScopes((current) =>
      current.includes(scope)
        ? current.filter((s) => s !== scope)
        : [...current, scope],
    );
  }

  return (
    <div className="card">
      <div className="page-header">
        <div>
          <strong>{displayName(subject)}</strong>
          <div className="mono muted">{subject.subject}</div>
        </div>
        <label className="wildcard">
          <input
            type="checkbox"
            checked={isAdmin}
            onChange={() =>
              isAdmin
                ? setScopes(scopes.filter((s) => s !== "*"))
                : confirmed(
                    `Grant "*" to ${displayName(subject)}? That is every scope, ` +
                      `including the ability to change everyone else's access.`,
                    () => setScopes(["*"]),
                  )
            }
          />
          <span>
            Full admin (<code>*</code>)
          </span>
        </label>
      </div>

      {/* Individual scopes are hidden under "*" rather than shown
          disabled: "*" already implies all of them, and rendering them
          unchecked next to a wildcard that grants them would misreport
          what this person can do. */}
      {!isAdmin && (
        <div className="scopes">
          {knownScopes.map((scope) => (
            <label key={scope}>
              <input
                type="checkbox"
                checked={scopes.includes(scope)}
                onChange={() => toggle(scope)}
              />
              <code>{scope}</code>
            </label>
          ))}
        </div>
      )}

      <div className="actions">
        <Button
          variant="primary"
          disabled={busy || !dirty}
          onClick={() => onSave(scopes)}
        >
          {busy ? "Saving…" : "Save"}
        </Button>
        {dirty && (
          <Button onClick={() => setScopes(subject.scopes)} disabled={busy}>
            Revert
          </Button>
        )}
      </div>
    </div>
  );
}
