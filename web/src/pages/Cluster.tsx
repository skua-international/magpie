// Cluster-wide admin: disk accounting and the Steam session.
//
// Steam sign-in is the QR flow (see SteamQrLogin) rather than the
// paste-a-refresh-token form this used to carry -- same flow magpiectl
// runs, and the token never passes through the browser at all.

import { admin } from "../api/clients";
import { SteamQrLogin } from "../components/SteamQrLogin";
import { Banner, Spinner, formatBytes } from "../components/ui";
import { useAsync } from "../components/useAsync";

export function Cluster() {
  const usage = useAsync(() => admin.getDiskUsage({}), []);

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

      <SteamQrLogin />
    </section>
  );
}
