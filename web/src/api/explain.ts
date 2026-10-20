/**
 * Client-side mirrors of the human-facing helpers in flux-core:
 * `Reason::explain()`, `PlaybackPlan::label()` (crates/flux-core/src/plan.rs)
 * and the `display_name()` of `Codec`, `Container` and `HdrFormat` (media.rs).
 *
 * The server sends `explain` sentences (with display names) in `PlayResponse`
 * and `PlanPreview`; these mirrors exist for the mock backend and the item
 * page's local "expected playback" sketch. They are checked against strings
 * produced by the Rust code itself (golden.test.ts), so server sentences are
 * shown as they arrive: no client-side prettifying.
 */
import type { Codec, Container, HdrFormat, PlanLabel, PlaybackPlan, Reason } from './types';

/** Rust's `{:.0}` rounds ties to even (2.5 → "2"); `toFixed` would not. */
function rustFixed0(x: number): string {
  const floor = Math.floor(x);
  const diff = x - floor;
  let r: number;
  if (diff > 0.5) r = floor + 1;
  else if (diff < 0.5) r = floor;
  else r = floor % 2 === 0 ? floor : floor + 1;
  return String(r);
}

/** Exact mirror of `Reason::explain()`. */
export function explainReason(r: Reason): string {
  switch (r.reason) {
    case 'container_unsupported':
      return `This device can't open ${containerName(r.container)} files directly, so flux repackages the streams (no quality loss)`;
    case 'video_codec_unsupported':
      return `This device can't decode ${codecName(r.codec)} video`;
    case 'video_profile_unsupported':
      return `This device can't decode ${codecName(r.codec)} ${r.profile} video`;
    case 'video_level_too_high':
      return `${codecName(r.codec)} level ${r.level} is above this device's limit (${r.max})`;
    case 'video_bit_depth_unsupported':
      return `This device can't decode ${r.bit_depth}-bit video`;
    case 'video_resolution_too_high':
      return `${r.width}×${r.height} is above this device's maximum resolution`;
    case 'hdr_unsupported':
      return `This display can't show ${hdrName(r.format)}, so flux tone-maps to SDR`;
    case 'dovi_profile_unsupported':
      return `This device can't play Dolby Vision profile ${r.profile}`;
    case 'audio_codec_unsupported':
      return `This device can't decode ${codecName(r.codec)} audio`;
    case 'audio_channels_too_many':
      return `${r.channels}-channel audio is more than this device supports (${r.max})`;
    case 'subtitle_needs_burn_in':
      return `${codecName(r.codec)} subtitles can't be shown by this device, so they're drawn into the picture`;
    case 'bitrate_too_high':
      return `The file's bitrate (${rustFixed0(r.bitrate / 1e6)} Mb/s) is above this connection's capacity (${rustFixed0(r.max / 1e6)} Mb/s)`;
    case 'no_index':
      return 'The file has no seek index, so flux transcodes it';
    case 'codec_not_packageable':
      return `${codecName(r.codec)} can't be carried in an HLS stream, so flux converts it`;
    case 'client_quirk':
      return r.note;
  }
}

/** Exact mirror of `PlaybackPlan::label()`. */
export function planLabel(plan: PlaybackPlan): PlanLabel {
  if (plan.delivery === 'progressive_file') return 'Direct play';
  if (plan.video.op === 'transcode') return 'Transcode';
  return plan.audio.some((a) => a.op.op !== 'copy') ? 'Audio conversion' : 'Direct stream';
}

/** Severity of a plan for colour coding: direct < remux < convert < transcode. */
export type PlanTone = 'direct' | 'stream' | 'convert' | 'transcode';

export function planTone(label: PlanLabel): PlanTone {
  switch (label) {
    case 'Direct play':
      return 'direct';
    case 'Direct stream':
      return 'stream';
    case 'Audio conversion':
      return 'convert';
    case 'Transcode':
      return 'transcode';
  }
}

// ───────────── display names (flux-core `display_name()`, checked by golden.test.ts) ─────────────

/** `Codec::display_name()`. */
export const CODEC_NAMES: Record<Codec, string> = {
  h264: 'H.264', hevc: 'HEVC', av1: 'AV1', vp9: 'VP9', vp8: 'VP8', mpeg2_video: 'MPEG-2', mpeg4_part2: 'MPEG-4 Part 2',
  vc1: 'VC-1', pro_res: 'ProRes', mjpeg: 'MJPEG',
  aac: 'AAC', ac3: 'AC-3', eac3: 'E-AC-3', true_hd: 'TrueHD', dts: 'DTS', flac: 'FLAC',
  opus: 'Opus', vorbis: 'Vorbis', mp3: 'MP3', mp2: 'MP2', pcm: 'PCM', alac: 'ALAC',
  subrip: 'SRT', ass: 'ASS', ssa: 'SSA', web_vtt: 'WebVTT', mov_text: 'MP4 text', pgs: 'PGS', vob_sub: 'VobSub',
  dvb_sub: 'DVB', other: 'Other',
};

/** Shorter names for dense edge-code labels (UI only). */
const CODEC_SHORT: Partial<Record<Codec, string>> = { mpeg4_part2: 'MPEG-4' };

/** `Container::display_name()`. */
export const CONTAINER_NAMES: Record<Container, string> = {
  matroska: 'MKV', web_m: 'WebM', mp4: 'MP4', mov: 'MOV', mpeg_ts: 'MPEG-TS', m2ts: 'M2TS', avi: 'AVI', unknown: 'Unknown',
};

/** `HdrFormat::display_name()`. */
export const HDR_NAMES: Record<HdrFormat, string> = {
  sdr: 'SDR', hdr10: 'HDR10', hdr10_plus: 'HDR10+', hlg: 'HLG', dolby_vision: 'Dolby Vision',
};

export const codecName = (c: Codec): string => CODEC_NAMES[c];
export const codecShort = (c: Codec): string => CODEC_SHORT[c] ?? CODEC_NAMES[c];
export const containerName = (c: Container): string => CONTAINER_NAMES[c];
export const hdrName = (h: HdrFormat): string => HDR_NAMES[h];

/** "Stereo", "5.1", "7.1"… */
export function channelLayout(channels: number | null): string {
  switch (channels) {
    case null:
      return '';
    case 1:
      return 'Mono';
    case 2:
      return 'Stereo';
    case 6:
      return '5.1';
    case 8:
      return '7.1';
    default:
      return `${channels} ch`;
  }
}
