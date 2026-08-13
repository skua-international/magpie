// Steam QR login, the same flow `magpiectl admin refresh-steam-auth`
// runs in a terminal.
//
// The cluster negotiates it, not this page: BeginSteamQrLogin opens the
// Steam connection server-side and returns a challenge URL to render,
// and the resulting refresh token goes straight from Steam to
// sync-daemon without ever passing through the browser. That is strictly
// better than the paste-a-token form this replaces.
//
// Polling is client-driven because the server's poll returns immediately
// -- sync-daemon does the blocking wait on a spawned task (its own
// poll_qr_login blocks until the phone confirms) and just records the
// outcome.

import { useCallback, useEffect, useRef, useState } from "react";
import QRCode from "qrcode";

import { admin, errorMessage } from "../api/clients";
import { Banner, Button, Spinner } from "./ui";

const POLL_INTERVAL_MS = 2000;

type Phase =
  | { kind: "idle" }
  | { kind: "starting" }
  | { kind: "waiting"; sessionId: string; challengeUrl: string }
  | { kind: "confirmed"; username: string }
  | { kind: "failed"; error: string };

export function SteamQrLogin() {
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  const begin = useCallback(async () => {
    setPhase({ kind: "starting" });
    try {
      const res = await admin.beginSteamQrLogin({});
      setPhase({
        kind: "waiting",
        sessionId: res.sessionId,
        challengeUrl: res.challengeUrl,
      });
    } catch (err) {
      setPhase({ kind: "failed", error: errorMessage(err) });
    }
  }, []);

  // Drawn to a canvas rather than fetched as an image: the challenge URL
  // is a live credential-ish value, and rendering it locally keeps it
  // from being sent anywhere else to be turned into a picture.
  useEffect(() => {
    if (phase.kind !== "waiting" || !canvasRef.current) return;
    QRCode.toCanvas(canvasRef.current, phase.challengeUrl, {
      width: 240,
      margin: 1,
    }).catch(() => {
      // The link below still works, so a canvas failure is not fatal.
    });
  }, [phase]);

  useEffect(() => {
    if (phase.kind !== "waiting") return;
    let cancelled = false;

    const timer = setInterval(async () => {
      try {
        const res = await admin.pollSteamQrLogin({ sessionId: phase.sessionId });
        if (cancelled) return;
        if (res.confirmed) {
          setPhase({ kind: "confirmed", username: res.username });
        }
      } catch (err) {
        if (cancelled) return;
        // sync-daemon restarts itself to pick up the new session, so the
        // session id stops being known shortly after it succeeds. A
        // not-found here is therefore the *expected* end of a successful
        // login, not a failure -- reporting it as an error would tell
        // the operator it broke when it actually worked.
        const message = errorMessage(err);
        if (/not_found|unknown QR login session/i.test(message)) {
          setPhase({ kind: "confirmed", username: "" });
        } else {
          setPhase({ kind: "failed", error: message });
        }
      }
    }, POLL_INTERVAL_MS);

    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [phase]);

  return (
    <div className="card">
      <h3>Steam session</h3>

      {phase.kind === "idle" && (
        <>
          <p className="muted">
            Sign in by scanning a QR code with the Steam mobile app. No password
            is involved, and the resulting token never passes through this
            browser — the cluster negotiates it directly with Steam.
          </p>
          <Button variant="primary" onClick={begin}>
            Start QR login
          </Button>
        </>
      )}

      {phase.kind === "starting" && <Spinner label="Contacting Steam…" />}

      {phase.kind === "waiting" && (
        <>
          <p className="muted">
            Scan with the Steam mobile app, then approve the sign-in.
          </p>
          <canvas ref={canvasRef} className="qr" />
          <p className="muted">
            {/* Useful on a device that already has Steam installed: the
                challenge URL deep-links into the app. */}
            Or open it directly:{" "}
            <a href={phase.challengeUrl} className="mono">
              {phase.challengeUrl}
            </a>
          </p>
          <Spinner label="Waiting for confirmation…" />
          <Button onClick={() => setPhase({ kind: "idle" })}>Cancel</Button>
        </>
      )}

      {phase.kind === "confirmed" && (
        <Banner kind="info">
          Steam session established{phase.username && ` as ${phase.username}`}.
          sync-daemon is restarting to pick it up.
        </Banner>
      )}

      {phase.kind === "failed" && (
        <>
          <Banner kind="error">{phase.error}</Banner>
          <Button onClick={begin}>Try again</Button>
        </>
      )}
    </div>
  );
}
