/** Mock playback bytes: the multi-audio variant has real demuxed renditions for in-session audio switching. */
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import type { ClientCaps } from '../api/types';
import { addOtherAudioTracks, predictPlan } from '../plan/predict';
import type { Fixtures } from './db';
import { MULTI_AUDIO_CLIP, mediaFor, type MockMedia } from './media';

const fx = JSON.parse(readFileSync(new URL('./fixtures.json', import.meta.url), 'utf8')) as Fixtures;
const probe = fx.profiles[MULTI_AUDIO_CLIP].probe;
const clip = { file: MULTI_AUDIO_CLIP, durationMs: probe.duration_ms };

const chrome: ClientCaps = {
  client: 'web/chrome-141/macos',
  delivery: ['progressive_file', 'hls_fmp4', 'hls_ts'],
  progressive_containers: ['mp4', 'web_m', 'matroska'],
  video: [{ codec: 'h264', profiles: ['high', 'main'], max_level: 51, max_bit_depth: 8, max_width: 3840, max_height: 2160, hdr: [], dovi_profiles: [] }],
  audio: [{ codec: 'aac', max_channels: 8 }],
  subtitles: ['web_vtt'],
  display_hdr: false,
  needs_demuxed_audio: false,
  max_bitrate: null,
};

const local: MockMedia = {
  mode: 'local',
  base: '/mock-media',
  index: {
    hls: {
      h264: { duration_ms: 7_200_021, prefetch: ['v/init.mp4', 'v/0.m4s'] },
      multi: { duration_ms: 60_021, prefetch: ['v/init.mp4', 'v/0.m4s'], audio: ['1', '2-aac6'] },
    },
    features: [MULTI_AUDIO_CLIP],
  },
};

const planFor = (audioTrack: number | null, caps = chrome) => {
  const { plan } = predictPlan({ probe, caps, audioTrack, subtitleTrack: null });
  addOtherAudioTracks(plan, probe, caps);
  return plan;
};

describe('mock media for the multi-audio clip', () => {
  it('direct play of the default track is the clip itself', () => {
    expect(mediaFor(local, planFor(null), probe, clip).url).toBe(`/mock-media/features/${MULTI_AUDIO_CLIP}`);
  });

  it('the Japanese track over HLS gets the demuxed variant, its master defaulting to that rendition', () => {
    const plan = planFor(2);
    expect(plan.audio.map((a) => a.track)).toEqual([2, 1]);
    const m = mediaFor(local, plan, probe, clip);
    expect(m.url).toBe('/mock-media/hls/multi/master-2-aac6.m3u8');
    expect(m.prefetch).toContain('/mock-media/hls/multi/a/2-aac6/init.mp4');
    expect(m.durationMs).toBe(60_021);
  });

  it('falls back to the muxed stand-in when a planned rendition isn’t on disk', () => {
    const safari = { ...chrome, audio: [...chrome.audio, { codec: 'ac3' as const, max_channels: 6 }], progressive_containers: ['mp4' as const] };
    const plan = planFor(2, safari); // AC-3 copied ("2"): not in the variant
    expect(mediaFor(local, plan, probe, clip).url).toBe('/mock-media/hls/h264/master.m3u8');
  });
});
