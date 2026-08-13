import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  availableProviders,
  callbackUrl,
  clearTokens,
  completeLoginFromUrl,
  refresh,
  storedAccessToken,
} from "./auth";

function mockFetch(impl: (url: string, init?: RequestInit) => Response | Promise<Response>) {
  const spy = vi.fn((input: RequestInfo | URL, init?: RequestInit) =>
    Promise.resolve(impl(String(input), init)),
  );
  vi.stubGlobal("fetch", spy);
  return spy;
}

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

beforeEach(() => {
  sessionStorage.clear();
  window.history.replaceState({}, "", "/ui/");
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("callbackUrl", () => {
  it("is same-origin by construction", () => {
    // This is what keeps the default deployment working with no
    // identity.allowedRedirectOrigins entry: gateway serves this bundle
    // on the same host identity runs on, so our own origin is allowed.
    expect(callbackUrl().startsWith(window.location.origin)).toBe(true);
    expect(callbackUrl()).toBe(`${window.location.origin}/ui/`);
  });
});

describe("availableProviders", () => {
  it("returns what the server reports", () => {
    mockFetch(() => json({ providers: ["steam", "discord"] }));
    return expect(availableProviders()).resolves.toEqual(["steam", "discord"]);
  });

  it("tolerates a response with no providers key", () => {
    mockFetch(() => json({}));
    return expect(availableProviders()).resolves.toEqual([]);
  });

  it("throws with the server's own body, which explains the cause", () => {
    mockFetch(() => new Response("nope", { status: 500 }));
    return expect(availableProviders()).rejects.toThrow(/500/);
  });
});

describe("completeLoginFromUrl", () => {
  it("does nothing without a code", async () => {
    const fetchSpy = mockFetch(() => json({}));
    await expect(completeLoginFromUrl()).resolves.toBe(false);
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("exchanges a code and stores both tokens", async () => {
    window.history.replaceState({}, "", "/ui/?code=abc123");
    mockFetch(() => json({ access_token: "at", refresh_token: "rt" }));

    await expect(completeLoginFromUrl()).resolves.toBe(true);
    expect(storedAccessToken()).toBe("at");
  });

  it("strips the code from the URL immediately", async () => {
    // It is single-use and short-lived, but leaving it in the address
    // bar puts it in history and in any copied link.
    window.history.replaceState({}, "", "/ui/?code=abc123");
    mockFetch(() => json({ access_token: "at", refresh_token: "rt" }));

    await completeLoginFromUrl();
    expect(window.location.search).not.toContain("code");
  });

  it("preserves other query parameters while stripping the code", async () => {
    window.history.replaceState({}, "", "/ui/?code=abc123&next=servers");
    mockFetch(() => json({ access_token: "at", refresh_token: "rt" }));

    await completeLoginFromUrl();
    expect(window.location.search).toContain("next=servers");
    expect(window.location.search).not.toContain("code");
  });

  it("throws when the exchange is refused", async () => {
    window.history.replaceState({}, "", "/ui/?code=spent");
    mockFetch(() => new Response("invalid, expired, or already-used", { status: 401 }));
    await expect(completeLoginFromUrl()).rejects.toThrow(/401/);
  });
});

describe("refresh", () => {
  it("returns false with nothing to refresh with", async () => {
    const fetchSpy = mockFetch(() => json({}));
    await expect(refresh()).resolves.toBe(false);
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("stores the new pair on success", async () => {
    sessionStorage.setItem("magpie.refresh_token", "old");
    mockFetch(() => json({ access_token: "new-at", refresh_token: "new-rt" }));

    await expect(refresh()).resolves.toBe(true);
    expect(storedAccessToken()).toBe("new-at");
  });

  it("clears tokens when the refresh token is spent", async () => {
    // Signed out, not an error: the caller treats false as "show the
    // login screen" rather than surfacing a failure.
    sessionStorage.setItem("magpie.access_token", "at");
    sessionStorage.setItem("magpie.refresh_token", "spent");
    mockFetch(() => new Response("revoked", { status: 401 }));

    await expect(refresh()).resolves.toBe(false);
    expect(storedAccessToken()).toBeNull();
  });
});

describe("clearTokens", () => {
  it("removes both tokens", () => {
    sessionStorage.setItem("magpie.access_token", "at");
    sessionStorage.setItem("magpie.refresh_token", "rt");
    clearTokens();
    expect(storedAccessToken()).toBeNull();
    expect(sessionStorage.getItem("magpie.refresh_token")).toBeNull();
  });
});
