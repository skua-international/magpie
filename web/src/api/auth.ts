// Browser login against services/identity.
//
// Reuses the exact flow `magpiectl` already uses (see cli/internal/auth):
// /auth/:provider/start returns a URL to send the user to, the provider
// bounces back to identity's callback, and identity redirects to our
// redirect_uri with a single-use exchange code that /auth/exchange trades
// for tokens. Nothing new server-side, which is what the issue asked for.
//
// The one place the browser differs from the CLI is where the redirect
// lands. The CLI listens on 127.0.0.1 and identity used to require that;
// a page served from a real hostname cannot. That's why this needs
// identity's origin allowlist -- and since gateway serves this bundle on
// the same host identity runs on, our own origin is allowed by default
// with no configuration (see identity.allowedRedirectOrigins).
//
// Tokens live in sessionStorage, deliberately not localStorage: they're
// scoped to the tab and evaporate when it closes, so a shared machine
// doesn't leave a usable session behind. Both are readable by injected
// script, so this is not XSS-proof -- an HttpOnly cookie session would
// be, and is the thing to build if this UI ever holds anything more
// dangerous than it does today. Doing that properly means a real session
// store in identity, which today keeps zero server-side session state
// (see services/identity/src/state.rs) and pins itself to replicas: 1
// for the in-process exchange-code map. That's a deliberate follow-up,
// not an oversight.

const ACCESS_KEY = "magpie.access_token";
const REFRESH_KEY = "magpie.refresh_token";

export interface TokenPair {
  access_token: string;
  refresh_token: string;
}

export function storedAccessToken(): string | null {
  return sessionStorage.getItem(ACCESS_KEY);
}

function storeTokens(pair: TokenPair) {
  sessionStorage.setItem(ACCESS_KEY, pair.access_token);
  sessionStorage.setItem(REFRESH_KEY, pair.refresh_token);
}

export function clearTokens() {
  sessionStorage.removeItem(ACCESS_KEY);
  sessionStorage.removeItem(REFRESH_KEY);
}

/// Where identity should send the browser back to. Same-origin by
/// construction -- it is literally this page -- which is what keeps the
/// default configuration working without an explicit allowlist entry.
export function callbackUrl(): string {
  return `${window.location.origin}/ui/`;
}

/// Which providers this deployment can actually complete a login with.
///
/// Asked rather than hardcoded: Discord/GitHub/Google are only enabled
/// when identity has credentials for them, and offering a button that
/// 404s with "provider not configured" is worse than not offering it.
/// Steam is always in the list -- it needs no app registration.
export async function availableProviders(): Promise<string[]> {
  const res = await fetch("/auth/providers", { headers: { Accept: "application/json" } });
  if (!res.ok) {
    throw new Error(`could not list login providers: ${res.status} ${await res.text()}`);
  }
  const body = (await res.json()) as { providers?: string[] };
  return body.providers ?? [];
}

export async function startLogin(provider: string): Promise<void> {
  const url = new URL(`/auth/${provider}/start`, window.location.origin);
  url.searchParams.set("redirect_uri", callbackUrl());

  const res = await fetch(url, { headers: { Accept: "application/json" } });
  if (!res.ok) {
    // identity answers a rejected redirect_uri with a 400 and a body
    // naming the problem; surfacing it beats a generic failure, since
    // the likely cause is a missing allowlist entry an operator has to
    // fix in values.
    throw new Error(`login could not be started: ${res.status} ${await res.text()}`);
  }
  const body = (await res.json()) as { url: string };
  window.location.assign(body.url);
}

/// Completes a login if the current URL carries an exchange code.
///
/// Returns true when it consumed one, so the caller knows to re-render as
/// signed in. The code is stripped from the URL immediately: it is
/// single-use and short-lived, but leaving it in the address bar puts it
/// in history and in any copied link.
export async function completeLoginFromUrl(): Promise<boolean> {
  const params = new URLSearchParams(window.location.search);
  const code = params.get("code");
  if (!code) return false;

  params.delete("code");
  const clean = params.toString();
  window.history.replaceState(
    {},
    "",
    window.location.pathname + (clean ? `?${clean}` : ""),
  );

  const res = await fetch("/auth/exchange", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ code }),
  });
  if (!res.ok) {
    throw new Error(`could not complete login: ${res.status} ${await res.text()}`);
  }
  storeTokens((await res.json()) as TokenPair);
  return true;
}

/// Trades the refresh token for a new pair. Returns false when there's
/// nothing to refresh with or the token is spent, which the caller treats
/// as "signed out" rather than an error worth showing.
export async function refresh(): Promise<boolean> {
  const refreshToken = sessionStorage.getItem(REFRESH_KEY);
  if (!refreshToken) return false;

  const res = await fetch("/auth/refresh", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ refresh_token: refreshToken }),
  });
  if (!res.ok) {
    clearTokens();
    return false;
  }
  storeTokens((await res.json()) as TokenPair);
  return true;
}
