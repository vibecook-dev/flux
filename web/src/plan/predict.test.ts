/** The client-side plan sketch against the feature clips' real probe facts. */
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { planLabel } from '../api/explain';
import type { ClientCaps, ProbeResult } from '../api/types';
import type { Fixtures } from '../mock/db';
import { predictPlan } from './predict';

const fx = JSON.parse(readFileSync(new URL('../mock/fixtures.json', import.meta.url), 'utf8')) as Fixtures;
const probe = (file: string): ProbeResult => fx.profiles[file].probe;

const chrome: ClientCaps = {
  client: 'web/chrome-141/macos',
  delivery: ['progressive_file', 'hls_fmp4', 'hls_ts'],
  progressive_containers: ['mp4', 'web_m'],
  video: [
    { codec: 'h264', profiles: ['high', 'main'], max_level: 51, max_bit_depth: 8, max_width: 3840, max_height: 2160, hdr: [], dovi_profiles: [] },
    { codec: 'hevc', profiles: ['main', 'main10'], max_level: 153, max_bit_depth: 10, max_width: 3840, max_height: 2160, hdr: ['hdr10', 'hlg'], dovi_profiles: [] },
    { codec: 'av1', profiles: ['main'], max_level: 13, max_bit_depth: 10, max_width: 3840, max_height: 2160, hdr: ['hdr10'], dovi_profiles: [] },
  ],
  audio: [
    { codec: 'aac', max_channels: 8 },
    { codec: 'opus', max_channels: 8 },
    { codec: 'flac', max_channels: 8 },
  ],
  subtitles: ['web_vtt'],
  display_hdr: false,
  needs_demuxed_audio: false,
  max_bitrate: null,
};

const plan = (file: string, caps = chrome, audioTrack: number | null = null, subtitleTrack: number | null = null) =>
  predictPlan({ probe: probe(file), caps, audioTrack, subtitleTrack });

describe('predictPlan on the feature clips', () => {
  it('MP4 with H.264/AAC plays directly', () => {
    const p = plan('h264_aac_faststart.mp4');
    expect(planLabel(p.plan)).toBe('Direct play');
    expect(p.plan.reasons).toEqual([]);
    expect(p.plan.est_cpu_ms_per_min).toBe(0);
  });

  it('MKV on a browser without Matroska is repackaged losslessly', () => {
    const p = plan('h264_flac.mkv');
    expect(planLabel(p.plan)).toBe('Direct stream');
    expect(p.plan.reasons).toEqual([{ reason: 'container_unsupported', container: 'matroska' }]);
    expect(p.plan.delivery).toBe('hls_fmp4');
  });

  it('TrueHD 7.1 becomes AAC 5.1 with the picture copied', () => {
    const p = plan('h264_truehd71.mkv');
    expect(planLabel(p.plan)).toBe('Audio conversion');
    expect(p.plan.video).toEqual({ op: 'copy', fixes: [] });
    expect(p.plan.audio[0].op).toEqual({ op: 'transcode', codec: 'aac', channels: 6, bitrate: 640_000 });
    expect(p.explain).toContain("This device can't decode TrueHD audio");
  });

  it('TrueHD a client can decode still converts: HLS can’t carry it', () => {
    const passthrough: ClientCaps = { ...chrome, audio: [...chrome.audio, { codec: 'true_hd', max_channels: 8 }] };
    const p = plan('h264_truehd71.mkv', passthrough);
    expect(p.plan.reasons).toContainEqual({ reason: 'codec_not_packageable', codec: 'true_hd' });
    expect(p.explain).toContain("TrueHD can't be carried in an HLS stream, so flux converts it");
    expect(p.plan.audio[0].op.op).toBe('transcode');
  });

  it('4K HDR10 HEVC with E-AC-3 copies video and converts audio', () => {
    const p = plan('hevc_main10_hdr10_eac3.mkv');
    expect(planLabel(p.plan)).toBe('Audio conversion');
    expect(p.plan.reasons.map((r) => r.reason)).toEqual(['container_unsupported', 'audio_codec_unsupported']);
  });

  it('HDR on a client that cannot show it is tone-mapped in a transcode', () => {
    const sdrOnly: ClientCaps = { ...chrome, video: chrome.video.map((v) => ({ ...v, hdr: [] })) };
    const p = plan('hevc_main10_hdr10_hvc1.mp4', sdrOnly);
    expect(planLabel(p.plan)).toBe('Transcode');
    expect(p.plan.video).toMatchObject({ op: 'transcode', codec: 'h264', width: 1920, height: 1080, tonemap: true });
  });

  it('MPEG-2 in a transport stream is transcoded, without claiming a lossless repackage', () => {
    const p = plan('mpeg2_ac3.ts');
    expect(planLabel(p.plan)).toBe('Transcode');
    expect(p.plan.reasons.map((r) => r.reason)).toEqual(['video_codec_unsupported', 'audio_codec_unsupported']);
  });

  it('H.264 in a transport stream is transcoded (no seek index), not "repackaged"', () => {
    const p = plan('h264_aac.m2ts');
    expect(planLabel(p.plan)).toBe('Transcode');
    expect(p.plan.reasons.map((r) => r.reason)).toContain('no_index');
    expect(p.plan.reasons.map((r) => r.reason)).not.toContain('container_unsupported');
  });

  it('picks the requested audio track', () => {
    const p = plan('h264_multi_audio_subs.mkv', chrome, 2);
    expect(p.plan.audio[0].track).toBe(2);
    expect(p.explain).toContain("This device can't decode AC-3 audio");
  });

  it('serves text subtitles as WebVTT sidecars', () => {
    const p = plan('h264_multi_audio_subs.mkv', chrome, null, 3);
    expect(p.plan.subtitles).toEqual([{ track: 3, op: { op: 'web_vtt' } }]);
  });

  it('burns in image subtitles the client cannot render', () => {
    const base = probe('h264_aac_faststart.mp4');
    const withPgs: ProbeResult = {
      ...base,
      tracks: [...base.tracks, { ...base.tracks[1], index: 2, kind: 'subtitle', codec: 'pgs', codec_tag: 'S_HDMV/PGS', channels: null, sample_rate: null }],
    };
    const p = predictPlan({ probe: withPgs, caps: chrome, audioTrack: null, subtitleTrack: 2 });
    expect(planLabel(p.plan)).toBe('Transcode');
    expect(p.plan.video).toMatchObject({ op: 'transcode', burn_subtitle: 2 });
  });

  it('MP3 is converted over HLS: MSE rejects it in fMP4', () => {
    const base = probe('h264_flac.mkv');
    const mp3: ProbeResult = { ...base, tracks: base.tracks.map((t) => (t.kind === 'audio' ? { ...t, codec: 'mp3', channels: 2 } : t)) };
    const p = predictPlan({ probe: mp3, caps: { ...chrome, audio: [...chrome.audio, { codec: 'mp3', max_channels: 2 }] }, audioTrack: null, subtitleTrack: null });
    expect(p.plan.reasons).toContainEqual({ reason: 'codec_not_packageable', codec: 'mp3' });
    expect(p.plan.audio[0].op).toEqual({ op: 'transcode', codec: 'aac', channels: 2, bitrate: 192_000 });
    expect(p.plan.video).toEqual({ op: 'copy', fixes: [] });
  });

  it('Matroska without Cues is still repackaged (flux-hls scans clusters)', () => {
    const p = predictPlan({ probe: { ...probe('h264_flac.mkv'), has_index: false }, caps: chrome, audioTrack: null, subtitleTrack: null });
    expect(planLabel(p.plan)).toBe('Direct stream');
    expect(p.plan.reasons.map((r) => r.reason)).not.toContain('no_index');
  });

  it('transcodes when the path is slower than the file', () => {
    const p = plan('h264_aac_faststart.mp4', { ...chrome, max_bitrate: 2_000_000 });
    expect(p.plan.reasons[0]).toMatchObject({ reason: 'bitrate_too_high', max: 2_000_000 });
    expect(planLabel(p.plan)).toBe('Transcode');
  });
});
