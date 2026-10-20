/** Capability detection against fake browser profiles (research/03 §2). */
import { describe, expect, it } from 'vitest';
import { clientId, detectCaps, isAppleWebKit, parseUserAgent, type MediaProbe } from './detect';
import { choosePlayerEngine } from './engine';

const UA = {
  chromeMac: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36',
  safariMac: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15',
  firefoxLinux: 'Mozilla/5.0 (X11; Linux x86_64; rv:145.0) Gecko/20100101 Firefox/145.0',
  edgeWin: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0',
  chromeIos: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/141.0 Mobile/15E148 Safari/604.1',
};

/** Build a probe from a list of supported codec substrings. */
function fakeProbe(opts: {
  ua: string;
  mse: string[];
  canPlay?: string[];
  nativeHls?: boolean;
  uhd?: boolean;
  hdr?: string[];
  hdrDisplay?: boolean;
  /** Container MIME prefixes this browser rejects outright. */
  rejects?: string[];
}): MediaProbe {
  const has = (list: string[], mime: string) => list.some((frag) => mime.includes(frag));
  return {
    userAgent: opts.ua,
    hasMse: opts.mse.length > 0,
    mseSupports: (mime) => has(opts.mse, mime),
    canPlayType: (mime) => {
      if (mime === 'application/vnd.apple.mpegurl') return opts.nativeHls ? 'maybe' : '';
      if ((opts.rejects ?? []).some((p) => mime.startsWith(p))) return '';
      return has(opts.canPlay ?? [], mime) ? 'maybe' : '';
    },
    decodingInfo: (cfg) => {
      const v = cfg.video;
      const ok = (v ? has(opts.mse.length ? opts.mse : (opts.canPlay ?? []), v.contentType) : true) && (!v || v.width < 3000 || opts.uhd === true);
      const hdrOk = !v?.transferFunction || (opts.hdr ?? []).includes(v.hdrMetadataType ?? v.transferFunction);
      return Promise.resolve({ supported: ok && hdrOk, smooth: true, powerEfficient: true });
    },
    hdrDisplay: opts.hdrDisplay ?? false,
  };
}

describe('detectCaps', () => {
  it('Chrome on an M1 Mac: HEVC + AV1 in hardware, no AC-3, HLS via MSE', async () => {
    const caps = await detectCaps(
      fakeProbe({
        ua: UA.chromeMac,
        mse: ['avc1', 'hvc1', 'av01', 'vp09', 'mp4a.40.2', 'opus', 'flac'],
        canPlay: ['video/mp4', 'video/webm', 'video/x-matroska'],
        nativeHls: true,
        uhd: true,
        hdr: ['smpteSt2086', 'hlg'],
      }),
    );
    expect(caps.client).toBe('web/chrome-141/macos');
    expect(caps.delivery).toEqual(['progressive_file', 'hls_fmp4', 'hls_ts']);
    expect(caps.progressive_containers).toEqual(['mp4', 'web_m', 'matroska']);
    const hevc = caps.video.find((v) => v.codec === 'hevc');
    expect(hevc?.profiles).toEqual(['main', 'main10']);
    expect(hevc?.max_width).toBe(3840);
    // A 4K60 hardware decoder takes HEVC level 5.2 (hevc_4k_60mbps.mkv).
    expect(hevc?.max_level).toBe(156);
    expect(hevc?.max_bit_depth).toBe(10);
    // SDR display, but macOS tone-maps HDR10/HLG itself → report decode support.
    expect(hevc?.hdr).toEqual(['hdr10', 'hlg']);
    // macOS tone-maps HDR itself, so it is listed above, but the display is SDR.
    expect(caps.display_hdr).toBe(false);
    expect(caps.audio.map((a) => a.codec)).toEqual(['aac', 'opus', 'flac']);
    expect(caps.audio.find((a) => a.codec === 'opus')?.max_channels).toBe(8);
    expect(caps.subtitles).toEqual(['web_vtt']);
    expect(caps.max_bitrate).toBeNull();
  });

  it('Safari on macOS: E-AC-3, Dolby Vision 8 and 5, stereo Opus, native HLS', async () => {
    const caps = await detectCaps(
      fakeProbe({
        ua: UA.safariMac,
        mse: ['avc1', 'hvc1', 'dvh1', 'mp4a.40.2', 'ac-3', 'ec-3', 'opus', 'flac', 'alac'],
        canPlay: ['video/mp4', 'video/quicktime', 'avc1', 'hvc1', 'dvh1', 'mp4a.40.2', 'ec-3'],
        nativeHls: true,
        uhd: true,
        hdr: ['smpteSt2086', 'smpteSt2094-40', 'hlg'],
        hdrDisplay: true,
        rejects: ['video/x-matroska', 'video/webm'],
      }),
    );
    expect(caps.client).toBe('web/safari-26/macos');
    const hevc = caps.video.find((v) => v.codec === 'hevc');
    expect(hevc?.dovi_profiles).toEqual([8, 5]);
    expect(hevc?.hdr).toEqual(['hdr10', 'hdr10_plus', 'hlg', 'dolby_vision']);
    expect(caps.display_hdr).toBe(true);
    expect(caps.audio.map((a) => a.codec)).toEqual(['aac', 'ac3', 'eac3', 'opus', 'flac', 'alac']);
    expect(caps.audio.find((a) => a.codec === 'opus')?.max_channels).toBe(2);
    expect(caps.progressive_containers).toEqual(['mp4', 'mov']);
  });

  it('Firefox on Linux without VA-API: no HEVC, no AC-3; HDR not reported on an SDR display', async () => {
    const caps = await detectCaps(
      fakeProbe({
        ua: UA.firefoxLinux,
        mse: ['avc1', 'av01', 'vp09', 'mp4a.40.2', 'opus', 'flac'],
        canPlay: ['video/mp4', 'video/webm', 'video/x-matroska'],
        uhd: true,
        hdr: ['smpteSt2086'],
      }),
    );
    expect(caps.client).toBe('web/firefox-145/linux');
    expect(caps.video.map((v) => v.codec)).toEqual(['h264', 'av1', 'vp9']);
    expect(caps.video.find((v) => v.codec === 'av1')?.hdr).toEqual([]);
    expect(caps.audio.map((a) => a.codec)).not.toContain('ac3');
  });

  it('caps 1080p decoders at 1920×1080', async () => {
    const caps = await detectCaps(fakeProbe({ ua: UA.chromeMac, mse: ['avc1', 'mp4a.40.2'], canPlay: ['video/mp4'], uhd: false }));
    const h264 = caps.video.find((v) => v.codec === 'h264');
    expect([h264?.max_width, h264?.max_height]).toEqual([1920, 1080]);
  });

  it('claims HEVC level 5.2 only when decodingInfo accepts 4K60', async () => {
    const caps = await detectCaps(fakeProbe({ ua: UA.chromeMac, mse: ['avc1', 'hvc1', 'mp4a.40.2'], canPlay: ['video/mp4'], uhd: false }));
    expect(caps.video.find((v) => v.codec === 'hevc')?.max_level).toBe(153);
  });
});

describe('player engine choice', () => {
  const env = (ua: string, hlsJs: boolean, native: boolean) => ({ userAgent: ua, hlsJsSupported: hlsJs, nativeHls: native });

  it('forces hls.js (MSE) on Chrome even though Chrome now ships native HLS', () => {
    expect(choosePlayerEngine('hls_fmp4', env(UA.chromeMac, true, true))).toBe('hls.js');
    expect(choosePlayerEngine('hls_fmp4', env(UA.edgeWin, true, true))).toBe('hls.js');
  });

  it('uses native HLS on Safari and every iOS browser', () => {
    expect(choosePlayerEngine('hls_fmp4', env(UA.safariMac, true, true))).toBe('native-hls');
    expect(choosePlayerEngine('hls_fmp4', env(UA.chromeIos, false, true))).toBe('native-hls');
  });

  it('plays progressive files directly and reports when nothing works', () => {
    expect(choosePlayerEngine('progressive_file', env(UA.firefoxLinux, true, false))).toBe('progressive');
    expect(choosePlayerEngine('hls_fmp4', env(UA.firefoxLinux, false, false))).toBeNull();
  });
});

describe('user agent parsing', () => {
  it('identifies browser, version and platform', () => {
    expect(parseUserAgent(UA.edgeWin)).toEqual({ browser: 'edge', version: 141, platform: 'windows' });
    expect(clientId(UA.chromeIos)).toBe('web/chrome-141/ios');
    expect(isAppleWebKit(UA.chromeMac)).toBe(false);
    expect(isAppleWebKit(UA.chromeIos)).toBe(true);
  });
});
