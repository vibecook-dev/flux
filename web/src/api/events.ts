/**
 * One shared EventSource for GET /api/v1/events, reference-counted across
 * subscribers. EventSource reconnects on its own; we surface the connection
 * state so the admin screen can show "Live" vs "Reconnecting".
 */
import { api, whenReady } from './client';
import type { FluxEvent } from './types';

export type EventsState = 'connecting' | 'open' | 'reconnecting' | 'closed';

type Listener = (e: FluxEvent) => void;
type StateListener = (s: EventsState) => void;

const listeners = new Set<Listener>();
const stateListeners = new Set<StateListener>();
let source: EventSource | null = null;
let state: EventsState = 'closed';

function setState(s: EventsState) {
  state = s;
  for (const l of stateListeners) l(s);
}

/** Parse one SSE `data:` payload; `null` for anything that isn't a known event. */
export function parseEvent(data: string): FluxEvent | null {
  try {
    const v = JSON.parse(data) as { type?: unknown };
    if (
      v &&
      (v.type === 'scan_progress' ||
        v.type === 'items_changed' ||
        v.type === 'source_availability' ||
        v.type === 'tailnet' ||
        v.type === 'changes' ||
        v.type === 'lenses' ||
        v.type === 'collections')
    ) {
      return v as FluxEvent;
    }
  } catch {
    // ignore malformed frames; the stream continues
  }
  return null;
}

let opening = false;

function open() {
  if (source || opening || typeof EventSource === 'undefined') return;
  opening = true;
  setState('connecting');
  void whenReady().then(() => {
    opening = false;
    if (listeners.size === 0 && stateListeners.size === 0) {
      setState('closed');
      return;
    }
    connect();
  });
}

function connect() {
  const es = new EventSource(api.eventsUrl());
  source = es;
  es.onopen = () => setState('open');
  es.onerror = () => setState(es.readyState === EventSource.CLOSED ? 'closed' : 'reconnecting');
  es.onmessage = (msg: MessageEvent<string>) => {
    const evt = parseEvent(msg.data);
    if (evt) for (const l of listeners) l(evt);
  };
}

function closeIfIdle() {
  if (listeners.size === 0 && stateListeners.size === 0 && source) {
    source.close();
    source = null;
    setState('closed');
  }
}

/** Subscribe to server events; returns the unsubscribe function. */
export function subscribeEvents(onEvent: Listener): () => void {
  listeners.add(onEvent);
  open();
  return () => {
    listeners.delete(onEvent);
    closeIfIdle();
  };
}

/** Observe the connection state (also keeps the stream open). */
export function subscribeEventsState(onState: StateListener): () => void {
  stateListeners.add(onState);
  open();
  onState(state);
  return () => {
    stateListeners.delete(onState);
    closeIfIdle();
  };
}
