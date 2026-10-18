import { useCallback, useEffect, useRef, useState, type DependencyList } from "react";
import { toMessage } from "../shared/errors";

export interface Options {
  skip?: boolean;
  onSkip?: "reset" | "keep";
  onError?: "message" | "emptyHits";
}

export interface SearchHits<T> {
  hits: T[];
  loading: boolean;
  error: string | null;
  setError: (msg: string) => void;
  clearHits: () => void;
}

// Shared search-hit fetch: one request per deps change, a cancelled flag keeps
// stale responses from applying, loading/error follow the request lifecycle.
// skip+onSkip cover the empty-query variants (reset to empty vs keep results),
// onError picks between surfacing the message and falling back to no hits.
export function useSearchHits<T>(
  run: () => Promise<T[]>,
  deps: DependencyList,
  opts: Options = {},
): SearchHits<T> {
  const runRef = useRef(run);
  runRef.current = run;
  const [hits, setHits] = useState<T[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (opts.skip) {
      if ((opts.onSkip ?? "reset") === "reset") {
        setHits([]);
        setError(null);
        setLoading(false);
      }
      return;
    }
    let cancelled = false;
    setLoading(true);
    setError(null);
    void (async () => {
      try {
        const result = await runRef.current();
        if (!cancelled) setHits(result);
      } catch (e) {
        if (!cancelled) {
          if ((opts.onError ?? "message") === "emptyHits") setHits([]);
          else setError(toMessage(e));
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, deps);

  const setErrorMessage = useCallback((msg: string) => setError(msg), []);
  const clearHits = useCallback(() => setHits([]), []);

  return {
    hits,
    loading,
    error,
    setError: setErrorMessage,
    clearHits,
  };
}
