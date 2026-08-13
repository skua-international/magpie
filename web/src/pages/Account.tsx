// The signed-in user's own identity and linked provider accounts.
//
// Linking uses exactly the flow `magpiectl account link` does: the same
// /auth/:provider/start endpoint, but with the current access token on
// the request. identity reads that token and attaches the newly
// authenticated provider to the existing user instead of creating a new
// one -- see its `start` handler, which is why that endpoint returns a
// URL to visit rather than a 302 (a bearer header can't ride along on a
// browser redirect).

import { useState } from "react";

import { availableProviders, linkProvider, whoAmI } from "../api/auth";
import { errorMessage } from "../api/clients";
import { Banner, Button, Spinner, Table } from "../components/ui";
import { useAsync } from "../components/useAsync";

export function Account() {
  const me = useAsync(() => whoAmI(), []);
  const providers = useAsync(() => availableProviders(), []);
  const [error, setError] = useState<string | null>(null);

  if (me.loading) return <Spinner label="Loading account…" />;
  if (me.error) return <Banner kind="error">{me.error}</Banner>;

  const linked = me.data?.accounts ?? [];
  const linkedProviders = new Set(linked.map((a) => a.provider));
  // Steam is always offered by the server, but a provider already linked
  // isn't worth offering again -- linking the same one twice is a no-op
  // at best.
  const linkable = (providers.data ?? []).filter((p) => !linkedProviders.has(p));

  return (
    <section>
      <header className="page-header">
        <h2>Account</h2>
      </header>

      {error && <Banner kind="error">{error}</Banner>}

      <p className="muted">
        Signed in as <span className="mono">{me.data?.subject}</span>
      </p>

      <h3>Linked accounts</h3>
      <Table
        rows={linked}
        rowKey={(a) => `${a.provider}:${a.provider_user_id}`}
        empty="No linked accounts."
        columns={[
          { header: "Provider", cell: (a) => a.provider },
          { header: "Name", cell: (a) => a.display_name || <span className="muted">—</span> },
          {
            header: "ID",
            cell: (a) => <span className="mono">{a.provider_user_id}</span>,
          },
        ]}
      />

      <h3>Link another</h3>
      {linkable.length === 0 ? (
        <p className="muted">Every configured provider is already linked.</p>
      ) : (
        <>
          <p className="muted">
            Signing in with a provider that's already attached to a different
            account merges the two.
          </p>
          <div className="actions">
            {linkable.map((provider) => (
              <Button
                key={provider}
                onClick={() =>
                  linkProvider(provider).catch((e) => setError(errorMessage(e)))
                }
              >
                Link {provider}
              </Button>
            ))}
          </div>
        </>
      )}
    </section>
  );
}
