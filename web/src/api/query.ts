/**
 * A deliberately small data cache: request de-duplication, prefetch, stale
 * time, and React subscription via useSyncExternalStore. It is what makes
 * "prefetch on hover/focus" and instant back/forward navigation work without
 * pulling in a 13 kB query library.
 */
import { useCallback, useEffect, useSyncExternalStore } from 'react';

export type QueryStatus = 'idle' | 'loading' | 'success' | 'error';

export interface QueryState<T> {
  status: QueryStatus;
  data: T | undefined;
  error: unknown;
  updatedAt: number;
}

interface Entry {
  state: QueryState<unknown>;
  promise: Promise<unknown> | null;
  listeners: Set<() => void>;
}

const IDLE: QueryState<never> = Object.freeze({ status: 'idle', data: undefined, error: undefined, updatedAt: 0 });
const cache = new Map<string, Entry>();

function entry(key: string): Entry {
  let e = cache.get(key);
  if (!e) {
    e = { state: IDLE, promise: null, listeners: new Set() };
    cache.set(key, e);
  }
  return e;
}

function setState(key: string, next: QueryState<unknown>) {
  const e = entry(key);
  e.state = next;
  for (const l of e.listeners) l();
}

/** Fetch (or join an in-flight fetch of) `key`. Rejects with the fetcher's error. */
export function fetchQuery<T>(key: string, fetcher: () => Promise<T>): Promise<T> {
  const e = entry(key);
  if (e.promise) return e.promise as Promise<T>;
  const prev = e.state;
  setState(key, { ...prev, status: prev.data === undefined ? 'loading' : prev.status });
  const p = fetcher().then(
    (data) => {
      e.promise = null;
      setState(key, { status: 'success', data, error: undefined, updatedAt: Date.now() });
      return data;
    },
    (error: unknown) => {
      e.promise = null;
      setState(key, { ...e.state, status: 'error', error });
      throw error;
    },
  );
  e.promise = p;
  return p;
}

/** Warm the cache (hover/focus intent). Never throws; skips fresh data. */
export function prefetchQuery<T>(key: string, fetcher: () => Promise<T>, staleMs = 30_000): void {
  const s = entry(key).state;
  if (s.status === 'success' && Date.now() - s.updatedAt < staleMs) return;
  fetchQuery(key, fetcher).catch(() => undefined);
}

/** Read whatever is cached for `key` without fetching. */
export function peekQuery<T>(key: string): T | undefined {
  return cache.get(key)?.state.data as T | undefined;
}

/** Seed or replace cached data (optimistic updates, SSE-driven updates). */
export function setQueryData<T>(key: string, data: T | ((prev: T | undefined) => T)): void {
  const prev = peekQuery<T>(key);
  const next = typeof data === 'function' ? (data as (p: T | undefined) => T)(prev) : data;
  setState(key, { status: 'success', data: next, error: undefined, updatedAt: Date.now() });
}

/** Mark keys starting with `prefix` stale; mounted queries refetch. */
export function invalidateQueries(prefix: string): void {
  for (const [key, e] of cache) {
    if (key.startsWith(prefix)) {
      setState(key, { ...e.state, updatedAt: 0 });
    }
  }
}

export interface UseQueryOptions {
  /** Don't fetch while false. */
  enabled?: boolean;
  /** Refetch on mount when older than this (default 30 s). */
  staleMs?: number;
}

export interface UseQueryResult<T> extends QueryState<T> {
  refetch: () => Promise<T>;
}

/** Subscribe to `key`, fetching when missing or stale. */
export function useQuery<T>(key: string, fetcher: () => Promise<T>, opts: UseQueryOptions = {}): UseQueryResult<T> {
  const { enabled = true, staleMs = 30_000 } = opts;
  const subscribe = useCallback(
    (cb: () => void) => {
      const e = entry(key);
      e.listeners.add(cb);
      return () => e.listeners.delete(cb);
    },
    [key],
  );
  const getSnapshot = useCallback(() => entry(key).state, [key]);
  const state = useSyncExternalStore(subscribe, getSnapshot, getSnapshot) as QueryState<T>;

  useEffect(() => {
    if (!enabled) return;
    const s = entry(key).state;
    const stale = s.status !== 'success' || Date.now() - s.updatedAt >= staleMs;
    if (stale && s.status !== 'error') fetchQuery(key, fetcher).catch(() => undefined);
    // `fetcher` is intentionally not a dependency: the key identifies the data.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, enabled, staleMs, state.updatedAt]);

  const refetch = useCallback(() => {
    const e = entry(key);
    e.promise = null;
    return fetchQuery(key, fetcher);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  return { ...state, refetch };
}

/** Test helper: forget everything. */
export function clearQueryCache(): void {
  cache.clear();
}
