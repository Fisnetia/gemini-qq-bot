import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError } from "./api";

/// Shared fetch-on-mount state for the pages.
export function useFetch<T>(load: () => Promise<T>, deps: unknown[] = []) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const loadRef = useRef(load);
  loadRef.current = load;

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      setData(await loadRef.current());
      setError(null);
    } catch (err) {
      setError(err instanceof ApiError ? err.code : "加载失败");
    } finally {
      setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  useEffect(() => {
    void reload();
  }, [reload]);

  return { data, error, loading, reload };
}

export function Loading() {
  return (
    <p role="status" className="muted">
      加载中…
    </p>
  );
}

export function ErrorBox({ message }: { message: string }) {
  return (
    <p role="alert" className="error-box">
      出错了：{message}
    </p>
  );
}

/// Polite live-region feedback for save/action results.
export function Notice({ text }: { text: string | null }) {
  if (!text) return null;
  return (
    <p role="status" aria-live="polite" className="notice">
      {text}
    </p>
  );
}

export function pct(fraction: number | null | undefined): string {
  if (fraction === null || fraction === undefined) return "—";
  return `${(fraction * 100).toFixed(1)}%`;
}

export function unixTime(ts: number | null | undefined): string {
  if (!ts) return "—";
  return new Date(ts * 1000).toLocaleString("zh-CN", { hour12: false });
}
