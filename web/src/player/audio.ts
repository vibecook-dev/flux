/**
 * Audio track switching (e2e P2 #4, PB-6). `PlayResponse.audio` lists the
 * session's HLS audio renditions, so the player switches between them inside
 * the session (hls.js `setAudioOption`, or the native `audioTracks` list on
 * Safari): no new `POST /play`, no new session, and the server doesn't warm
 * every rendition again. A new session at the current position is still
 * needed when:
 *
 * - the file plays directly (progressive): the browser plays the file's
 *   default track, and a track it can't decode needs the server (HLS);
 * - the session has no rendition of that track;
 * - hls.js would have to leave a FLAC or Opus SourceBuffer for another
 *   codec: hls.js 1.7.3 misses the `changeType` (it knows `fLaC`/`Opus`, the
 *   browser reports `flac`/`opus`), the next append fails and playback
 *   stalls (docs/prototype/hls.md, Phase C.1 #3).
 */
import type { AudioPlan, AudioRef, Codec, PlayResponse } from '../api/types';
import { toBcp47 } from '../api/tracks';
import type { PlayerEngine } from '../caps/engine';

/** Codecs hls.js can't switch away from inside one MediaSource. */
const STICKY_IN_HLSJS: readonly Codec[] = ['flac', 'opus'];

export type RestartReason = 'direct-play' | 'not-in-session' | 'codec-change';

/** One audio switch as the viewer felt it: click → new audio buffered (in session) or → first frame (restart). */
export interface AudioSwitchSample {
  ms: number;
  mode: 'in-session' | 'restart';
  /** Rendition key switched to ("2-aac2"), or "track N" for a restart without one. */
  to: string;
}

export type AudioSwitch = { kind: 'none' } | { kind: 'in-session'; ref: AudioRef } | { kind: 'restart'; reason: RestartReason };

/** The rendition key flux-hls gives an audio plan entry: "1", "1-aac2", "1-eac36". */
export function renditionKey(a: AudioPlan): string {
  if (a.op.op === 'copy') return String(a.track);
  return `${a.track}-${a.op.codec === 'eac3' ? 'eac3' : 'aac'}${a.op.channels}`;
}

/** The rendition a session starts on (the server's `default`), or null for direct play. */
export function startingRendition(res: PlayResponse): AudioRef | null {
  return res.audio.find((a) => a.default) ?? res.audio[0] ?? null;
}

/** The source track playing now: the selected rendition's, else the plan's first. */
export function playingTrack(res: PlayResponse, current: AudioRef | null): number | null {
  return current?.track ?? res.plan.audio[0]?.track ?? null;
}

/**
 * How to switch to source audio track `track`, given the rendition playing
 * now (`current`, null for direct play) and the engine that plays the session.
 */
export function planAudioSwitch(res: PlayResponse, current: AudioRef | null, track: number, engine: PlayerEngine | null): AudioSwitch {
  if (playingTrack(res, current) === track) return { kind: 'none' };
  if (res.plan.delivery === 'progressive_file' || engine === 'progressive') return { kind: 'restart', reason: 'direct-play' };
  const candidates = res.audio.filter((a) => a.track === track);
  if (candidates.length === 0) return { kind: 'restart', reason: 'not-in-session' };
  if (engine === 'hls.js' && current && STICKY_IN_HLSJS.includes(current.codec)) {
    const same = candidates.find((a) => a.codec === current.codec);
    return same ? { kind: 'in-session', ref: same } : { kind: 'restart', reason: 'codec-change' };
  }
  // Plan order: the server lists the rendition it prefers for a track first.
  return { kind: 'in-session', ref: candidates[0] };
}

/** The hint after a track in the audio menu when picking it restarts the stream. */
export function restartNote(reason: RestartReason): string {
  return reason === 'codec-change' ? 'restarts the stream (codec change)' : 'restarts the stream';
}

/** The audio menu's footnote: on direct play, every other track takes a new stream. */
export function audioMenuNote(res: PlayResponse): string | null {
  return res.plan.delivery === 'progressive_file'
    ? 'This file is playing directly, so another track restarts the stream from here (converted if this browser can’t play it).'
    : null;
}

/** The bits of an hls.js `MediaPlaylist` that identify an audio rendition. */
export interface HlsAudioTrack {
  url: string;
  name: string;
  groupId: string;
}

/**
 * The hls.js audio track (across all groups) that carries `ref`: by its URL
 * (`…/a/{rendition}/index.m3u8`), else by its NAME, which flux-hls writes as
 * `AudioRef.label`.
 */
export function findHlsAudioTrack<T extends HlsAudioTrack>(tracks: readonly T[], ref: AudioRef): T | undefined {
  const suffix = `/a/${ref.rendition}/index.m3u8`;
  const path = (u: string) => {
    try {
      return new URL(u, 'http://x').pathname;
    } catch {
      return u.split('?')[0];
    }
  };
  return tracks.find((t) => path(t.url).endsWith(suffix)) ?? tracks.find((t) => t.name === ref.label);
}

/** The native (Safari) `AudioTrack` subset flux uses; TypeScript's DOM types don't have it. */
export interface NativeAudioTrack {
  label: string;
  language: string;
  enabled: boolean;
}

/** The native audio track for `ref`: by label (the playlist NAME), else the only one in its language. */
export function findNativeAudioTrack<T extends NativeAudioTrack>(tracks: readonly T[], ref: AudioRef): T | undefined {
  const byLabel = tracks.find((t) => t.label === ref.label);
  if (byLabel) return byLabel;
  const lang = toBcp47(ref.language);
  const sameLang = lang ? tracks.filter((t) => t.language.toLowerCase().split('-')[0] === lang) : [];
  return sameLang.length === 1 ? sameLang[0] : undefined;
}
