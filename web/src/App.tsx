import { useEffect, useState } from "react";

import {
  availableProviders,
  clearTokens,
  completeLoginFromUrl,
  startLogin,
  storedAccessToken,
} from "./api/auth";
import { errorMessage } from "./api/clients";
import { Banner, Button } from "./components/ui";
import { Access } from "./pages/Access";
import { Account } from "./pages/Account";
import { Cluster } from "./pages/Cluster";
import { Missions } from "./pages/Missions";
import { Secrets } from "./pages/Secrets";
import { ModSources } from "./pages/ModSources";
import { Servers } from "./pages/Servers";

// Hash-based, not the History API: gateway serves this bundle from a
// ServeDir whose fallback is index.html, so real paths would work -- but
// the hash keeps every route a single static file request with no
// server-side route table to keep in step, which matters more for an app
// mounted under a prefix (/ui) that the router would otherwise have to
// know about in two places.
const TABS = {
  servers: { label: "Servers", render: () => <Servers /> },
  mods: { label: "Mod sources", render: () => <ModSources /> },
  missions: { label: "Missions", render: () => <Missions /> },
  access: { label: "Access", render: () => <Access /> },
  secrets: { label: "Secrets", render: () => <Secrets /> },
  cluster: { label: "Cluster", render: () => <Cluster /> },
  account: { label: "Account", render: () => <Account /> },
} as const;

type Tab = keyof typeof TABS;

function currentTab(): Tab {
  const hash = window.location.hash.replace(/^#\/?/, "");
  return hash in TABS ? (hash as Tab) : "servers";
}

export function App() {
  const [signedIn, setSignedIn] = useState(storedAccessToken() !== null);
  const [tab, setTab] = useState<Tab>(currentTab());
  const [error, setError] = useState<string | null>(null);
  const [booting, setBooting] = useState(true);
  // Server-provided rather than hardcoded -- see availableProviders.
  const [providers, setProviders] = useState<string[]>([]);

  useEffect(() => {
    const onHash = () => setTab(currentTab());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  useEffect(() => {
    // Runs before anything renders as signed-out: landing back here from
    // a provider means the URL carries an exchange code, and showing a
    // login screen for the instant it takes to redeem it would be a
    // flash of exactly the wrong thing.
    completeLoginFromUrl()
      .then((completed) => {
        if (completed) setSignedIn(true);
      })
      .catch((err) => setError(errorMessage(err)))
      .finally(() => setBooting(false));
  }, []);

  useEffect(() => {
    if (signedIn) return;
    availableProviders()
      .then(setProviders)
      .catch((err) => setError(errorMessage(err)));
  }, [signedIn]);

  if (booting) return <main className="shell">Loading…</main>;

  if (!signedIn) {
    return (
      <main className="shell login">
        <h1>magpie</h1>
        {error && <Banner kind="error">{error}</Banner>}
        <p className="muted">Sign in to manage this cluster.</p>
        {providers.length === 0 && !error && <p className="muted">Loading providers…</p>}
        <div className="actions">
          {providers.map((provider) => (
            <Button
              key={provider}
              variant={provider === "steam" ? "primary" : "default"}
              onClick={() => startLogin(provider).catch((e) => setError(errorMessage(e)))}
            >
              {provider}
            </Button>
          ))}
        </div>
      </main>
    );
  }

  return (
    <main className="shell">
      <nav>
        <h1>magpie</h1>
        <ul>
          {Object.entries(TABS).map(([key, { label }]) => (
            <li key={key}>
              <a href={`#/${key}`} className={key === tab ? "active" : undefined}>
                {label}
              </a>
            </li>
          ))}
        </ul>
        <Button
          onClick={() => {
            clearTokens();
            setSignedIn(false);
          }}
        >
          Sign out
        </Button>
      </nav>
      {error && <Banner kind="error">{error}</Banner>}
      {/* Keyed on the tab so switching remounts rather than reusing the
          previous page's loading/error state. */}
      <div key={tab} className="content">
        {TABS[tab].render()}
      </div>
    </main>
  );
}
