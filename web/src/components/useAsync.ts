// One hook for "load something over RPC, show it, let the user retry".
//
// Every page has the same three states and the same reload-after-mutation
// need, so this exists instead of each one repeating a
// useState/useEffect/try-catch triple.

import { useCallback, useEffect, useState } from "react";

import { errorMessage } from "../api/clients";

export interface Async<T> {
  data: T | null;
  error: string | null;
  loading: boolean;
  reload: () => void;
}

export function useAsync<T>(load: () => Promise<T>, deps: unknown[] = []): Async<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [nonce, setNonce] = useState(0);

  // eslint-disable-next-line react-hooks/exhaustive-deps
  const run = useCallback(load, deps);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    run()
      .then((value) => {
        // Guarded so a slow response that lands after the component is
        // gone (or after a newer request superseded it) can't overwrite
        // fresher state or warn about setting state while unmounted.
        if (!cancelled) {
          setData(value);
          setError(null);
        }
      })
      .catch((err) => {
        if (!cancelled) setError(errorMessage(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [run, nonce]);

  return { data, error, loading, reload: () => setNonce((n) => n + 1) };
}

/// Runs a mutation, then reloads whatever list it affected.
///
/// Returns the error rather than throwing so a failed action leaves the
/// page intact with a message, instead of unmounting into an error
/// boundary -- these are all "click a button, something might refuse"
/// operations, not unrecoverable faults.
export function useAction(reload: () => void) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const run = useCallback(
    async (fn: () => Promise<unknown>) => {
      setBusy(true);
      setError(null);
      try {
        await fn();
        reload();
      } catch (err) {
        setError(errorMessage(err));
      } finally {
        setBusy(false);
      }
    },
    [reload],
  );

  return { run, busy, error };
}
