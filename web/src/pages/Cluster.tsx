// Cluster-wide admin: disk accounting and the Steam session refresh.
//
// The Steam half deliberately only accepts an already-negotiated refresh
// token. The interactive username+password+Steam Guard negotiation stays
// client-side in magpiectl (see AdminService.RefreshSteamAuth's own proto
// doc) -- moving it into a browser would mean the account password
// crossing this service, which is exactly what that design avoids. So
// this page takes the token magpiectl produces rather than reimplementing
// the login.

import { useState } from "react";

import { admin } from "../api/clients";
import { Banner, Button, Field, Spinner, formatBytes } from "../components/ui";
import { useAction, useAsync } from "../components/useAsync";

export function Cluster() {
  const usage = useAsync(() => admin.getDiskUsage({}), []);
  const action = useAction(usage.reload);
  const [username, setUsername] = useState("");
  const [refreshToken, setRefreshToken] = useState("");
  const [done, setDone] = useState(false);

  return (
    <section>
      <header className="page-header">
        <h2>Cluster</h2>
      </header>

      <h3>Disk usage</h3>
      {usage.loading && <Spinner label="Loading disk usage…" />}
      {usage.error && <Banner kind="error">{usage.error}</Banner>}
      {usage.data && (
        <dl className="stats">
          <div>
            <dt>Mods</dt>
            <dd>{formatBytes(usage.data.modsBytes)}</dd>
          </div>
          <div>
            <dt>Missions</dt>
            <dd>{formatBytes(usage.data.missionsBytes)}</dd>
          </div>
          <div>
            <dt>Game files</dt>
            <dd>{formatBytes(usage.data.gameFilesBytes)}</dd>
          </div>
          <div>
            <dt>Total</dt>
            <dd>{formatBytes(usage.data.totalBytes)}</dd>
          </div>
        </dl>
      )}

      <h3>Steam session</h3>
      <p className="muted">
        Negotiate the token with <code>magpiectl admin refresh-steam-auth</code>; it
        never sends the account password here.
      </p>
      {action.error && <Banner kind="error">{action.error}</Banner>}
      {done && !action.error && (
        <Banner kind="info">Steam session replaced. sync-daemon is restarting to pick it up.</Banner>
      )}
      <form
        className="card"
        onSubmit={(e) => {
          e.preventDefault();
          setDone(false);
          action.run(async () => {
            await admin.refreshSteamAuth({
              username: username.trim(),
              refreshToken: refreshToken.trim(),
            });
            setRefreshToken("");
            setDone(true);
          });
        }}
      >
        <Field label="Steam username">
          <input value={username} required onChange={(e) => setUsername(e.target.value)} />
        </Field>
        <Field label="Refresh token">
          <input
            type="password"
            value={refreshToken}
            required
            onChange={(e) => setRefreshToken(e.target.value)}
          />
        </Field>
        <Button type="submit" variant="primary" disabled={action.busy}>
          {action.busy ? "Applying…" : "Replace session"}
        </Button>
      </form>
    </section>
  );
}
