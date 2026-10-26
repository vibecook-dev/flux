/**
 * What the island shows besides "where you are": short live events that grow out
 * of it and settle back (a scan finished, a drive went offline), and one ongoing
 * thing it keeps as a ring (a scan in progress, the film you left halfway).
 * Any module can post here; the island renders it.
 */
import { useSyncExternalStore } from 'react';
import type { ArtRef, Id } from '../api/types';
import type { IconName } from '../components/Icon';

export interface Activity {
  id: string;
  /** Shown when there are no thumbnails. */
  icon?: IconName;
  title: string;
  detail?: string;
  /** Up to three posters. */
  thumbs?: (ArtRef | null)[];
  action?: { label: string; run: () => void };
  /** How long it stays open (default 5.2 s; hovering pauses it). */
  ms?: number;
  /** Called once, when the island starts showing it. */
  onShown?: () => void;
}

export interface LiveRing {
  kind: 'scanning' | 'session';
  /** 0–1, or null for an indeterminate ring. */
  progress: number | null;
  label: string;
  aria: string;
  itemId?: Id;
  run?: () => void;
}

interface State {
  queue: Activity[];
  current: Activity | null;
  ring: LiveRing | null;
}

const listeners = new Set<() => void>();
let state: State = { queue: [], current: null, ring: null };
let ready = false;
let next: ReturnType<typeof setTimeout> | null = null;

function set(patch: Partial<State>) {
  state = { ...state, ...patch };
  for (const l of listeners) l();
}

function pump() {
  if (!ready || state.current || state.queue.length === 0 || next) return;
  const [first, ...rest] = state.queue;
  set({ current: first, queue: rest });
  first.onShown?.();
}

/** Show a live event in the island. An event with the same id replaces a queued one. */
export function postActivity(a: Activity): void {
  set({ queue: [...state.queue.filter((q) => q.id !== a.id), a] });
  pump();
}

/** The island can show events now (nothing covers it, it isn't open). */
export function setIslandReady(r: boolean): void {
  ready = r;
  if (!r && state.current) set({ current: null });
  pump();
}

/** The current event is done; the next one follows after a breath. */
export function dismissActivity(): void {
  if (!state.current) return;
  set({ current: null });
  next = setTimeout(() => {
    next = null;
    pump();
  }, 450);
}

export function setRing(ring: LiveRing | null): void {
  set({ ring });
}

export function currentRing(): LiveRing | null {
  return state.ring;
}

/** The current state (tests, non-React callers). */
export function liveState(): State {
  return state;
}

export function useLive(): State {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => state,
    () => state,
  );
}

/** Test helper. */
export function resetLive(): void {
  if (next) clearTimeout(next);
  next = null;
  ready = false;
  state = { queue: [], current: null, ring: null };
}
