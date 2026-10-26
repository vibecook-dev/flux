/**
 * Where mock playback gets real bytes. flux-server will serve
 * /media/{plan}/… itself; until then the mock points the player at:
 *
 * - local: HLS remuxes of the feature clips and the clips themselves, served by
 *   the Vite dev/preview middleware at /mock-media (scripts/make-mock-media.sh).
 * - remote: a public HLS test stream (Mux's Big Buck Bunny), when the local
 *   media isn't there (e.g. a static build on another machine).
 */
import type { PlaybackPlan, ProbeResult } from '../api/types';
import { renditionKey } from '../player/audio';

export interface LocalIndex {
  /** `audio`: demuxed audio renditions (a/{rendition}/), for variants that have them. */
  hls: Record<string, { duration_ms: number; prefetch: string[]; audio?: string[] }>;
  features: string[];
}

/** The clip the `multi` variant (demuxed audio renditions) is cut from. */
export const MULTI_AUDIO_CLIP = 'h264_multi_audio_subs.mkv';

export type MockMedia = { mode: 'local'; base: string; index: LocalIndex } | { mode: 'remote' };

export const PUBLIC_HLS = 'https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8';

/**
 * Probe for /mock-media/index.json (served by vite.config.ts when the SSD is present).
 * A build served below a sub-path (the product site's live demo) looks below it,
 * so a site can host media for its demo there.
 */
export async function detectMockMedia(base = `${import.meta.env.BASE_URL}mock-media`): Promise<MockMedia> {
  try {
    const res = await fetch(`${base}/index.json`, { cache: 'no-store' });
    if (res.ok) return { mode: 'local', base, index: (await res.json()) as LocalIndex };
  } catch {
    // fall through to the public stream
  }
  return { mode: 'remote' };
}

export interface MediaChoice {
  url: string;
  prefetch: string[];
  /** Actual length of the bytes we point at (may differ from the item's runtime). */
  durationMs: number | null;
}

/**
 * Pick bytes that match the plan: the clip file itself for direct play, else
 * the HLS remux whose video codec matches what the plan delivers.
 */
export function mediaFor(media: MockMedia, plan: PlaybackPlan, probe: ProbeResult, clip?: { file: string; durationMs: number | null }): MediaChoice {
  if (media.mode === 'remote') return { url: PUBLIC_HLS, prefetch: [], durationMs: null };
  if (plan.delivery === 'progressive_file' && clip && media.index.features.includes(clip.file)) {
    // The clip's own length, not the catalog runtime it stands in for.
    return { url: `${media.base}/features/${clip.file}`, prefetch: [], durationMs: clip.durationMs };
  }
  // The multi-audio clip over HLS with every planned rendition on disk: real
  // demuxed renditions, so the player switches audio inside the session. The
  // master is the one whose DEFAULT is the plan's first rendition.
  const multi = media.index.hls.multi;
  const keys = plan.audio.map(renditionKey);
  if (clip?.file === MULTI_AUDIO_CLIP && multi?.audio && plan.video.op === 'copy' && keys.length > 0 && keys.every((k) => multi.audio?.includes(k))) {
    const root = `${media.base}/hls/multi`;
    const prefetch = ['v/init.mp4', 'v/0.m4s', `a/${keys[0]}/init.mp4`, `a/${keys[0]}/0.m4s`];
    return { url: `${root}/master-${keys[0]}.m3u8`, prefetch: prefetch.map((p) => `${root}/${p}`), durationMs: multi.duration_ms };
  }
  const srcVideo = probe.tracks.find((t) => t.kind === 'video')?.codec;
  const delivered = plan.video.op === 'transcode' ? plan.video.codec : srcVideo;
  const variant = delivered === 'hevc' && media.index.hls.hevc ? 'hevc' : delivered === 'av1' && media.index.hls.av1 ? 'av1' : 'h264';
  const v = media.index.hls[variant];
  const root = `${media.base}/hls/${variant}`;
  return { url: `${root}/master.m3u8`, prefetch: (v?.prefetch ?? []).map((p) => `${root}/${p}`), durationMs: v?.duration_ms ?? null };
}
