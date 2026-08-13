// Connect-ES clients against the generated stubs.
//
// Imported straight from generated/ts -- the same descriptors magpiectl's
// Go clients and any other consumer use, so there is no hand-written REST
// layer to keep in sync with the protos.
//
// One transport for every service: after the single-entrypoint change all
// of them live behind one origin, routed by RPC path, so baseUrl is just
// this page's own origin.

import { createClient, type Interceptor } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";

import { ServerService } from "../../../generated/ts/controller/v1/controller_pb";
import {
  AdminService,
  MissionService,
  ModSourceService,
} from "../../../generated/ts/registry/v1/registry_pb";

import { clearTokens, refresh, storedAccessToken } from "./auth";

/// Attaches the access token, and retries once through a refresh when the
/// server says it's no longer good.
///
/// The retry is what stops a token expiring mid-session from looking like
/// a random failure: access tokens are short-lived by design, so any
/// long-lived tab will hit this. Only one retry, and only after a
/// successful refresh -- a second 401 means the refresh token is spent
/// too, and the right answer is to sign out rather than loop.
const authInterceptor: Interceptor = (next) => async (req) => {
  const token = storedAccessToken();
  if (token) req.header.set("Authorization", `Bearer ${token}`);

  try {
    return await next(req);
  } catch (err) {
    if (!isUnauthenticated(err)) throw err;

    if (!(await refresh())) {
      clearTokens();
      throw err;
    }
    const fresh = storedAccessToken();
    if (fresh) req.header.set("Authorization", `Bearer ${fresh}`);
    return await next(req);
  }
};

/// Connect surfaces the middleware's 401 as code "unauthenticated"; the
/// string check is a fallback for a transport-level failure that never
/// became a ConnectError.
function isUnauthenticated(err: unknown): boolean {
  const code = (err as { code?: unknown })?.code;
  if (code === "unauthenticated" || code === 16) return true;
  return err instanceof Error && /unauthenticated|401/i.test(err.message);
}

const transport = createConnectTransport({
  baseUrl: window.location.origin,
  interceptors: [authInterceptor],
});

export const servers = createClient(ServerService, transport);
export const modSources = createClient(ModSourceService, transport);
export const missions = createClient(MissionService, transport);
export const admin = createClient(AdminService, transport);

/// Turns any thrown value into something worth putting in front of a
/// person. Connect errors carry a useful `message`; anything else gets
/// stringified rather than rendering "[object Object]".
export function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return typeof err === "string" ? err : JSON.stringify(err);
}
