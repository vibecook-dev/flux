/**
 * One-round-trip play (research/03 §11). On the Play click we start, in
 * parallel: capability detection (normally already cached), POST /api/v1/play,
 * the player route's code, and hls.js. The player screen then picks up the
 * in-flight request instead of starting from scratch, and TTFF is measured
 * from the click itself.
 */
import { loadPlayer } from '../app/routes';
import { navigateWhenLoaded } from '../app/router';
import { api } from '../api/client';
import type { Id, PlayRequest, PlayResponse } from '../api/types';
import { getClientCaps, isAppleWebKit } from '../caps/detect';
import { resolveUrl } from '../config';

export interface PlayOptions {
  assetId?: Id | null;
  audioTrack?: number | null;
  subtitleTrack?: number | null;
  /** `null` = resume point. */
  startMs?: number | null;
}

export interface PendingPlay {
  itemId: Id;
  /** `performance.now()` at the user's click. */
  t0: number;
  request: Promise<PlayResponse>;
  options: PlayOptions;
  /** When a player first took it; a revisit via history later starts afresh. */
  takenAt?: number;
}

const pending = new Map<string, PendingPlay>();

/** Warm everything Play needs (hover/focus on a Play button). */
export function preloadPlayer(): void {
  void loadPlayer();
  if (!isAppleWebKit(navigator.userAgent)) void import('hls.js');
  void getClientCaps();
}

/** Build and send the play request (also used when the player is opened directly). */
export function requestPlay(itemId: Id, o: PlayOptions): Promise<PlayResponse> {
  return getClientCaps().then((caps) => {
    const req: PlayRequest = {
      item_id: itemId,
      asset_id: o.assetId ?? null,
      caps,
      audio_track: o.audioTrack ?? null,
      subtitle_track: o.subtitleTrack ?? null,
      start_ms: o.startMs ?? null,
    };
    return api.play(req);
  });
}

/** Warm the HTTP cache with the URLs the server says the player needs first. */
export function prefetchUrls(urls: string[]): void {
  for (const u of urls) {
    // `priority` is a hint (Chromium); other engines ignore it.
    void fetch(resolveUrl(u), { priority: 'high' }).catch(() => undefined);
  }
}

export function startPlay(itemId: Id, options: PlayOptions = {}): void {
  const t0 = performance.now();
  preloadPlayer();
  const request = requestPlay(itemId, options);
  // Prefetch init + first segment the moment the plan arrives, even if the
  // player's code is still downloading.
  request.then((res) => prefetchUrls(res.prefetch)).catch(() => undefined);
  const token = Math.random().toString(36).slice(2, 10);
  // Old entries are dropped here rather than on take, so React StrictMode's
  // double-mounted effects in development still find their request.
  pending.clear();
  pending.set(token, { itemId, t0, request, options });
  const qs = new URLSearchParams({ t: token });
  if (options.assetId) qs.set('asset', String(options.assetId));
  if (options.audioTrack !== undefined && options.audioTrack !== null) qs.set('audio', String(options.audioTrack));
  if (options.subtitleTrack !== undefined && options.subtitleTrack !== null) qs.set('sub', String(options.subtitleTrack));
  if (options.startMs !== undefined && options.startMs !== null) qs.set('start', String(options.startMs));
  // Navigate once the player's code is in (instant after a hover; a tap or
  // a remote's OK has no hover), so it renders without suspending; the
  // current screen stays up meanwhile, as a transition would keep it.
  navigateWhenLoaded(`/play/${itemId}?${qs.toString()}`);
}

/** The in-flight request started by the click that opened the player, if any. */
export function takePendingPlay(token: string | null): PendingPlay | undefined {
  const p = token ? pending.get(token) : undefined;
  if (!p) return undefined;
  const now = performance.now();
  if (p.takenAt !== undefined && now - p.takenAt > 2000) return undefined;
  p.takenAt ??= now;
  return p;
}
