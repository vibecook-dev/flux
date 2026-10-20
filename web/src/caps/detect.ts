/**
 * Builds flux-core `ClientCaps` from runtime probes (research/03 §2, §11):
 *
 * - `MediaSource.isTypeSupported` (or `ManagedMediaSource` on Safari) says
 *   whether a codec string can be fed through MSE, i.e. HLS via hls.js.
 * - `HTMLMediaElement.canPlayType` covers progressive files and native HLS.
 * - `navigator.mediaCapabilities.decodingInfo` answers the questions the
 *   type strings can't: 4K decode, PQ/HLG transfer, HDR metadata.
 * - `matchMedia('(dynamic-range: high)')` says whether the display is HDR.
 *
 * All browser access goes through `MediaProbe` so the logic is unit-tested
 * with fake Chrome/Safari/Firefox profiles (detect.test.ts).
 */
import type { AudioCap, ClientCaps, Codec, Container, DeliveryFormat, HdrFormat, VideoCap } from '../api/types';

export interface DecodingResult {
  supported: boolean;
  smooth: boolean;
  powerEfficient: boolean;
}

/** Everything detection needs from the browser. */
export interface MediaProbe {
  userAgent: string;
  /** MSE (or ManagedMediaSource) is available. */
  hasMse: boolean;
  /** `isTypeSupported` of the available MSE implementation; false when none. */
  mseSupports(mime: string): boolean;
  /** `<video>.canPlayType`. */
  canPlayType(mime: string): '' | 'maybe' | 'probably';
  /** `mediaCapabilities.decodingInfo`; resolves unsupported on error. */
  decodingInfo(config: MediaDecodingConfiguration): Promise<DecodingResult>;
  /** `(dynamic-range: high)` matches. */
  hdrDisplay: boolean;
}

interface VideoCandidate {
  codec: Codec;
  profile: string;
  bitDepth: number;
  /** Level idc in flux-core's convention (H.264 51 = 5.1; HEVC 153 = 5.1). */
  level: number;
  mime: string;
  /**
   * A higher level is claimed only when `decodingInfo` accepts a stream that
   * needs it: MSE type checks accept any level string, so they can't tell.
   */
  verify?: { width: number; height: number; bitrate: number; framerate: number };
}

/** HEVC level 5.2: 4K at 60 Mb/s (Main tier), e.g. 4K60 remuxes and UHD Blu-ray-style encodes. */
const UHD_L52 = { width: 3840, height: 2160, bitrate: 60_000_000, framerate: 60 };

// Codec strings: RFC 6381 (avc1/hvc1), AV1 ISOBMFF binding (av01), VP9 codec string (vp09).
const VIDEO_CANDIDATES: VideoCandidate[] = [
  { codec: 'h264', profile: 'high', bitDepth: 8, level: 51, mime: 'video/mp4; codecs="avc1.640033"' },
  { codec: 'h264', profile: 'high', bitDepth: 8, level: 41, mime: 'video/mp4; codecs="avc1.640029"' },
  { codec: 'h264', profile: 'main', bitDepth: 8, level: 40, mime: 'video/mp4; codecs="avc1.4d4028"' },
  { codec: 'hevc', profile: 'main', bitDepth: 8, level: 153, mime: 'video/mp4; codecs="hvc1.1.6.L153.90"' },
  { codec: 'hevc', profile: 'main10', bitDepth: 10, level: 153, mime: 'video/mp4; codecs="hvc1.2.4.L153.90"' },
  // Level 5.1 alone sent hevc_4k_60mbps.mkv (L5.2) through a 4K → 1080p transcode
  // on an M1 Max that decodes it in hardware (found by the e2e test).
  { codec: 'hevc', profile: 'main', bitDepth: 8, level: 156, mime: 'video/mp4; codecs="hvc1.1.6.L156.90"', verify: UHD_L52 },
  { codec: 'hevc', profile: 'main10', bitDepth: 10, level: 156, mime: 'video/mp4; codecs="hvc1.2.4.L156.90"', verify: UHD_L52 },
  { codec: 'av1', profile: 'main', bitDepth: 10, level: 13, mime: 'video/mp4; codecs="av01.0.13M.10"' },
  { codec: 'av1', profile: 'main', bitDepth: 8, level: 8, mime: 'video/mp4; codecs="av01.0.08M.08"' },
  { codec: 'vp9', profile: 'profile2', bitDepth: 10, level: 51, mime: 'video/mp4; codecs="vp09.02.51.10"' },
  { codec: 'vp9', profile: 'profile0', bitDepth: 8, level: 41, mime: 'video/mp4; codecs="vp09.00.41.08"' },
];

interface AudioCandidate {
  codec: Codec;
  mime: string;
  /** Channels the browser decodes (it downmixes to the output device itself). */
  channels: number;
}

const AUDIO_CANDIDATES: AudioCandidate[] = [
  { codec: 'aac', mime: 'audio/mp4; codecs="mp4a.40.2"', channels: 8 },
  { codec: 'ac3', mime: 'audio/mp4; codecs="ac-3"', channels: 6 },
  { codec: 'eac3', mime: 'audio/mp4; codecs="ec-3"', channels: 8 },
  { codec: 'opus', mime: 'audio/mp4; codecs="opus"', channels: 8 },
  { codec: 'flac', mime: 'audio/mp4; codecs="flac"', channels: 8 },
  { codec: 'mp3', mime: 'audio/mp4; codecs="mp4a.69"', channels: 2 },
  { codec: 'alac', mime: 'audio/mp4; codecs="alac"', channels: 8 },
  { codec: 'true_hd', mime: 'audio/mp4; codecs="mlpa"', channels: 8 },
  { codec: 'dts', mime: 'audio/mp4; codecs="dtsc"', channels: 6 },
];

const PROGRESSIVE: [Container, string][] = [
  ['mp4', 'video/mp4; codecs="avc1.640029, mp4a.40.2"'],
  ['mov', 'video/quicktime; codecs="avc1.640029, mp4a.40.2"'],
  ['web_m', 'video/webm; codecs="vp9, opus"'],
  // Chrome 145+ and Firefox 145+ play Matroska progressively (research/03 §2.1).
  ['matroska', 'video/x-matroska; codecs="avc1.640029, mp4a.40.2"'],
];

/** Engine family from a user agent string. */
export type Browser = 'chrome' | 'edge' | 'firefox' | 'safari' | 'other';
export type Platform = 'macos' | 'windows' | 'linux' | 'android' | 'ios' | 'chromeos' | 'other';

export function parseUserAgent(ua: string): { browser: Browser; version: number; platform: Platform } {
  let browser: Browser = 'other';
  let version = 0;
  const pick = (re: RegExp) => Number(re.exec(ua)?.[1] ?? 0);
  if (/Edg\//.test(ua)) {
    browser = 'edge';
    version = pick(/Edg\/(\d+)/);
  } else if (/Firefox\//.test(ua)) {
    browser = 'firefox';
    version = pick(/Firefox\/(\d+)/);
  } else if (/(Chrome|CriOS)\//.test(ua)) {
    browser = 'chrome';
    version = pick(/(?:Chrome|CriOS)\/(\d+)/);
  } else if (/Version\/[\d.]+.*Safari\//.test(ua)) {
    browser = 'safari';
    version = pick(/Version\/(\d+)/);
  }
  let platform: Platform = 'other';
  if (/iPhone|iPad|iPod/.test(ua)) platform = 'ios';
  else if (/Android/.test(ua)) platform = 'android';
  else if (/CrOS/.test(ua)) platform = 'chromeos';
  else if (/Mac OS X|Macintosh/.test(ua)) platform = 'macos';
  else if (/Windows/.test(ua)) platform = 'windows';
  else if (/Linux/.test(ua)) platform = 'linux';
  return { browser, version, platform };
}

/** flux-core `ClientCaps.client`, e.g. "web/chrome-141/macos" (quirks-table key). */
export function clientId(ua: string): string {
  const { browser, version, platform } = parseUserAgent(ua);
  return `web/${browser}${version ? `-${version}` : ''}/${platform}`;
}

/** Apple's WebKit (Safari, and every iOS browser): use native HLS there. */
export function isAppleWebKit(ua: string): boolean {
  return /AppleWebKit/.test(ua) && !/(Chrome|Chromium|Edg|OPR)\//.test(ua);
}

const UHD = { width: 3840, height: 2160, bitrate: 40_000_000, framerate: 24 };

/**
 * Detect capabilities. Each candidate costs one synchronous type check plus at
 * most four `decodingInfo` calls; the whole run takes a few milliseconds.
 */
export async function detectCaps(probe: MediaProbe): Promise<ClientCaps> {
  const { platform } = parseUserAgent(probe.userAgent);
  const appleWebKit = isAppleWebKit(probe.userAgent);
  const nativeHls = probe.canPlayType('application/vnd.apple.mpegurl') !== '';
  // Can this candidate play in the delivery we'd actually use (MSE, or native HLS on Apple)?
  const playable = (mime: string) =>
    (probe.hasMse && probe.mseSupports(mime)) || ((appleWebKit || !probe.hasMse) && nativeHls && probe.canPlayType(mime) !== '');
  const decodeType = probe.hasMse ? 'media-source' : 'file';

  // Browsers on macOS/iOS tone-map HDR10/HLG to SDR displays themselves, so
  // reporting decode support there avoids a needless server tone-map transcode.
  const clientToneMaps = platform === 'macos' || platform === 'ios';

  const video = new Map<Codec, VideoCap>();
  for (const c of VIDEO_CANDIDATES) {
    if (!playable(c.mime)) continue;
    if (c.verify && !(await probe.decodingInfo({ type: decodeType, video: { contentType: c.mime, ...c.verify } })).supported) continue;
    let cap = video.get(c.codec);
    if (!cap) {
      cap = { codec: c.codec, profiles: [], max_level: null, max_bit_depth: null, max_width: 1920, max_height: 1080, hdr: [], dovi_profiles: [] };
      video.set(c.codec, cap);
    }
    if (!cap.profiles.includes(c.profile)) cap.profiles.push(c.profile);
    cap.max_level = Math.max(cap.max_level ?? 0, c.level);
    cap.max_bit_depth = Math.max(cap.max_bit_depth ?? 0, c.bitDepth);

    const uhd = await probe.decodingInfo({ type: decodeType, video: { contentType: c.mime, ...UHD } });
    if (uhd.supported) {
      cap.max_width = UHD.width;
      cap.max_height = UHD.height;
    }
    if (c.bitDepth === 10 && (c.codec === 'hevc' || c.codec === 'av1' || c.codec === 'vp9')) {
      const hdrChecks: [HdrFormat, VideoConfiguration][] = [
        ['hdr10', { contentType: c.mime, ...UHD, transferFunction: 'pq', colorGamut: 'rec2020', hdrMetadataType: 'smpteSt2086' }],
        ['hdr10_plus', { contentType: c.mime, ...UHD, transferFunction: 'pq', colorGamut: 'rec2020', hdrMetadataType: 'smpteSt2094-40' }],
        ['hlg', { contentType: c.mime, ...UHD, transferFunction: 'hlg', colorGamut: 'rec2020' }],
      ];
      for (const [fmt, cfg] of hdrChecks) {
        if (cap.hdr.includes(fmt)) continue;
        const r = await probe.decodingInfo({ type: decodeType, video: cfg });
        if (r.supported && (probe.hdrDisplay || clientToneMaps)) cap.hdr.push(fmt);
      }
    }
  }

  // Dolby Vision: profile 8 (HDR10-compatible base) and profile 5 (no fallback).
  const hevc = video.get('hevc');
  if (hevc) {
    for (const [profile, mime] of [
      [8, 'video/mp4; codecs="dvh1.08.06"'],
      [5, 'video/mp4; codecs="dvh1.05.06"'],
    ] as const) {
      if (playable(mime)) hevc.dovi_profiles.push(profile);
    }
    if (hevc.dovi_profiles.length > 0 && (probe.hdrDisplay || clientToneMaps)) hevc.hdr.push('dolby_vision');
  }

  const audio: AudioCap[] = [];
  for (const a of AUDIO_CANDIDATES) {
    if (!playable(a.mime)) continue;
    // Safari decodes Opus as stereo only (research/03 §2.2).
    const channels = a.codec === 'opus' && appleWebKit ? 2 : a.channels;
    audio.push({ codec: a.codec, max_channels: channels });
  }

  const progressive: Container[] = [];
  for (const [container, mime] of PROGRESSIVE) {
    if (probe.canPlayType(mime) !== '') progressive.push(container);
  }

  const delivery: DeliveryFormat[] = ['progressive_file'];
  if (probe.hasMse || nativeHls) delivery.push('hls_fmp4');
  // hls.js transmuxes MPEG-TS; native HLS plays it directly.
  if (probe.hasMse || nativeHls) delivery.push('hls_ts');

  return {
    client: clientId(probe.userAgent),
    delivery,
    progressive_containers: progressive,
    video: [...video.values()],
    audio,
    // The display alone (research/03 §8). `video[].hdr` above already folds in
    // platform tone-mapping, which is what the planner's copy/tone-map choice uses.
    display_hdr: probe.hdrDisplay,
    // <track> renders WebVTT; ASS/PGS would need JASSUB/libbitsub (not bundled yet).
    subtitles: ['web_vtt'],
    // Web players mux or demux freely; only Roku-class clients need demuxed audio.
    needs_demuxed_audio: false,
    // Unknown here: navigator.connection.downlink is capped at 10 Mb/s by spec,
    // so the server's path classification (LAN / direct / relay) decides.
    max_bitrate: null,
  };
}

/** The real browser, wrapped as a MediaProbe. */
export function browserProbe(): MediaProbe {
  const video = document.createElement('video');
  // Safari 17+ exposes ManagedMediaSource (and on iOS, only that).
  const w = window as unknown as { ManagedMediaSource?: typeof MediaSource; MediaSource?: typeof MediaSource };
  const mse = w.ManagedMediaSource ?? w.MediaSource;
  const mc = navigator.mediaCapabilities as MediaCapabilities | undefined;
  return {
    userAgent: navigator.userAgent,
    hasMse: typeof mse?.isTypeSupported === 'function',
    mseSupports: (mime) => {
      try {
        return mse?.isTypeSupported(mime) ?? false;
      } catch {
        return false;
      }
    },
    canPlayType: (mime) => video.canPlayType(mime),
    decodingInfo: async (config) => {
      if (!mc) return { supported: false, smooth: false, powerEfficient: false };
      try {
        const r = await mc.decodingInfo(config);
        return { supported: r.supported, smooth: r.smooth, powerEfficient: r.powerEfficient };
      } catch {
        // Invalid or unknown configurations throw TypeError in some engines.
        return { supported: false, smooth: false, powerEfficient: false };
      }
    },
    hdrDisplay: typeof matchMedia === 'function' && matchMedia('(dynamic-range: high)').matches,
  };
}

let cached: Promise<ClientCaps> | null = null;

/** Detected once per page load; started at boot so it is ready before Play. */
export function getClientCaps(): Promise<ClientCaps> {
  cached ??= detectCaps(browserProbe());
  return cached;
}
