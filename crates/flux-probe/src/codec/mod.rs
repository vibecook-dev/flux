//! Bit-level codec configuration parsers.
//!
//! All hand-rolled over [`crate::bytes::Bits`] rather than pulling in
//! `h264-reader` / `hevc_parser` / `scuffle-*`: we need a small, read-only
//! subset (profile, level, bit depth, size, VUI colour/timing, a few SEI
//! payloads), it must never panic on hostile input, and it must work on a
//! partial buffer. Each parser returns `None` on malformed input; callers turn
//! that into a warning, never an error.

pub(crate) mod audio;
pub(crate) mod av1;
pub(crate) mod dovi;
pub(crate) mod h264;
pub(crate) mod hevc;
pub(crate) mod mpegvideo;
pub(crate) mod riff;
pub(crate) mod sei;

use flux_core::{
    Codec, ColorInfo, ContentLightLevel, DoviConfig, MasteringDisplay, Rational, Track, TrackKind,
};

/// What a video codec configuration (or in-band parameter sets) tells us.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct VideoCfg {
    pub profile: Option<String>,
    pub level: Option<u16>,
    pub bit_depth: Option<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Sample (pixel) aspect ratio stated by the bitstream (H.264/HEVC VUI,
    /// MPEG-2 sequence header) or, for container-level configs, the container.
    pub sar: Option<Rational>,
    pub frame_rate: Option<Rational>,
    pub interlaced: Option<bool>,
    pub color: Option<ColorInfo>,
    pub mastering: Option<MasteringDisplay>,
    pub cll: Option<ContentLightLevel>,
    /// HDR10+ (ST 2094-40 T.35 SEI / AV1 metadata) seen.
    pub hdr10plus: bool,
    /// Dolby Vision RPU (HEVC NAL 62 / AV1 T.35) seen.
    pub dovi_rpu: bool,
    pub dovi: Option<DoviConfig>,
    /// NAL length field size for length-prefixed (ISO/Matroska) samples.
    pub nal_len_size: Option<usize>,
}

impl VideoCfg {
    /// Fold `other` into `self`, keeping values already present.
    pub(crate) fn absorb(&mut self, other: VideoCfg) {
        macro_rules! keep {
            ($($f:ident),*) => { $( if self.$f.is_none() { self.$f = other.$f; } )* };
        }
        keep!(
            profile,
            level,
            bit_depth,
            width,
            height,
            sar,
            frame_rate,
            interlaced,
            mastering,
            cll,
            dovi,
            nal_len_size
        );
        self.color = crate::hdr::merge_color(self.color, other.color);
        self.hdr10plus |= other.hdr10plus;
        self.dovi_rpu |= other.dovi_rpu;
    }
}

/// What an audio codec configuration or first frame tells us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AudioCfg {
    /// Refined codec (e.g. AC-3 sync frame that is really E-AC-3).
    pub codec: Option<Codec>,
    pub profile: Option<String>,
    pub channels: Option<u8>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub bitrate: Option<u64>,
}

/// A track with everything unknown.
pub(crate) fn new_track(index: u32, id: u64, kind: TrackKind, codec: Codec, tag: impl Into<String>) -> Track {
    Track {
        index,
        id,
        kind,
        codec,
        codec_tag: tag.into(),
        profile: None,
        level: None,
        bit_depth: None,
        width: None,
        height: None,
        frame_rate: None,
        interlaced: None,
        color: None,
        hdr: None,
        channels: None,
        sample_rate: None,
        language: None,
        title: None,
        default: false,
        forced: false,
        hearing_impaired: false,
        bitrate: None,
        sample_aspect_ratio: None,
    }
}

/// Codec-level values win for size/depth/profile (they are what decoders and
/// ffprobe report); container values only fill gaps. Colour is merged with
/// the container taking precedence (see [`crate::hdr::merge_color`]), so the
/// caller applies container colour *after* this.
pub(crate) fn apply_video(t: &mut Track, cfg: &VideoCfg) {
    if cfg.profile.is_some() {
        t.profile.clone_from(&cfg.profile);
    }
    t.level = cfg.level.or(t.level);
    t.bit_depth = cfg.bit_depth.or(t.bit_depth);
    t.width = cfg.width.filter(|w| *w > 0).or(t.width);
    t.height = cfg.height.filter(|h| *h > 0).or(t.height);
    t.frame_rate = t.frame_rate.or(cfg.frame_rate);
    t.interlaced = t.interlaced.or(cfg.interlaced);
    t.color = crate::hdr::merge_color(t.color, cfg.color);
    // Unlike size, the container's aspect ratio wins (as in ffprobe's
    // `av_guess_sample_aspect_ratio`): callers set it before this.
    t.sample_aspect_ratio = t.sample_aspect_ratio.or(cfg.sar);
}

/// Container values win for audio (they are explicit), codec config fills gaps;
/// a refined codec always wins.
pub(crate) fn apply_audio(t: &mut Track, cfg: &AudioCfg) {
    if let Some(c) = cfg.codec {
        t.codec = c;
    }
    if cfg.profile.is_some() {
        t.profile.clone_from(&cfg.profile);
    }
    t.channels = t.channels.or(cfg.channels);
    t.sample_rate = t.sample_rate.or(cfg.sample_rate);
    t.bit_depth = t.bit_depth.or(cfg.bit_depth);
    t.bitrate = t.bitrate.or(cfg.bitrate);
    normalize_audio_depth(t);
}

/// Bit depth is only meaningful for lossless/PCM audio; containers write
/// the decoder's float sample size (32) for lossy codecs, which is noise.
pub(crate) fn normalize_audio_depth(t: &mut Track) {
    if t.kind == TrackKind::Audio
        && !matches!(t.codec, Codec::Pcm | Codec::Flac | Codec::Alac | Codec::TrueHd)
    {
        t.bit_depth = None;
    }
}

/// Exact frame rate from a frame duration in nanoseconds, snapped to the
/// common NTSC/film rationals (Matroska stores 41708333 ns for 24000/1001).
pub(crate) fn rate_from_ns(ns: u64) -> Option<Rational> {
    if ns == 0 {
        return None;
    }
    let fps = 1e9 / ns as f64;
    Some(snap_rate(fps).unwrap_or_else(|| reduce(1_000_000_000, ns)))
}

/// Rate from a `timescale / delta` pair (MP4 stts, VUI timing), snapped.
pub(crate) fn rate_from_ticks(timescale: u64, delta: u64) -> Option<Rational> {
    if timescale == 0 || delta == 0 {
        return None;
    }
    let fps = timescale as f64 / delta as f64;
    if !(0.5..=1000.0).contains(&fps) {
        return None;
    }
    Some(snap_rate(fps).unwrap_or_else(|| reduce(timescale, delta)))
}

/// H.264 Table E-1 / H.265 Table E-1 `aspect_ratio_idc` → SAR (index 0 =
/// unspecified; 255 = `Extended_SAR`, read from the stream).
const PIXEL_ASPECT: [(u32, u32); 17] = [
    (0, 1),
    (1, 1),
    (12, 11),
    (10, 11),
    (16, 11),
    (40, 33),
    (24, 11),
    (20, 11),
    (32, 11),
    (80, 33),
    (18, 11),
    (15, 11),
    (64, 33),
    (160, 99),
    (4, 3),
    (3, 2),
    (2, 1),
];

/// The VUI `aspect_ratio_info` of H.264 and HEVC (same syntax and table):
/// reads `aspect_ratio_info_present_flag` and what follows. `Ok(None)` when
/// absent, unspecified or invalid.
pub(crate) fn vui_aspect(b: &mut crate::bytes::Bits<'_>) -> Option<Option<Rational>> {
    if !b.flag()? {
        return Some(None);
    }
    let idc = b.u8(8)?;
    Some(if idc == 255 {
        let w = b.u(16)?;
        let h = b.u(16)?;
        sar(u64::from(w), u64::from(h))
    } else {
        PIXEL_ASPECT
            .get(usize::from(idc))
            .and_then(|&(n, d)| sar(u64::from(n), u64::from(d)))
    })
}

/// A sample aspect ratio `num:den`, reduced as ffmpeg does; `None` when
/// either side is 0 (ffmpeg's "undefined").
pub(crate) fn sar(num: u64, den: u64) -> Option<Rational> {
    if num == 0 || den == 0 {
        return None;
    }
    let (n, d) = av_reduce(u128::from(num), u128::from(den), i32::MAX as u128);
    match (u32::try_from(n), u32::try_from(d)) {
        (Ok(num), Ok(den)) if num > 0 && den > 0 => Some(Rational { num, den }),
        _ => None,
    }
}

/// FFmpeg's `av_reduce`: `num/den` in lowest terms when both fit in `max`,
/// else the closest continued-fraction approximation that does. Ported
/// exactly so that rounded ratios (Matroska `DisplayWidth` 853 for 720 px)
/// come out the way ffprobe prints them.
pub(crate) fn av_reduce(num: u128, den: u128, max: u128) -> (u128, u128) {
    fn gcd(a: u128, b: u128) -> u128 {
        if b == 0 {
            a
        } else {
            gcd(b, a % b)
        }
    }
    let (mut a0n, mut a0d) = (0u128, 1u128);
    let (mut a1n, mut a1d) = (1u128, 0u128);
    let g = gcd(num, den);
    let (mut num, mut den) = (
        num.checked_div(g).unwrap_or(num),
        den.checked_div(g).unwrap_or(den),
    );
    if num <= max && den <= max {
        a1n = num;
        a1d = den;
        den = 0;
    }
    while den != 0 {
        let mut x = num / den;
        let next_den = num - den * x;
        let a2n = x * a1n + a0n;
        let a2d = x * a1d + a0d;
        if a2n > max || a2d > max {
            // As in C: each bound applies only when its divisor is non-zero.
            if let Some(bound) = (max - a0n).checked_div(a1n) {
                x = bound;
            }
            if let Some(bound) = (max - a0d).checked_div(a1d) {
                x = x.min(bound);
            }
            if den * (2 * x * a1d + a0d) > num * a1d {
                a1n = x * a1n + a0n;
                a1d = x * a1d + a0d;
            }
            break;
        }
        a0n = a1n;
        a0d = a1d;
        a1n = a2n;
        a1d = a2d;
        num = den;
        den = next_den;
    }
    (a1n, a1d)
}

fn snap_rate(fps: f64) -> Option<Rational> {
    const COMMON: [(u32, u32); 14] = [
        (24000, 1001),
        (24, 1),
        (25, 1),
        (30000, 1001),
        (30, 1),
        (48, 1),
        (50, 1),
        (60000, 1001),
        (60, 1),
        (100, 1),
        (120000, 1001),
        (120, 1),
        (15, 1),
        (12, 1),
    ];
    COMMON
        .iter()
        .find(|(n, d)| ((*n as f64 / *d as f64) - fps).abs() / fps < 2e-4)
        .map(|&(num, den)| Rational { num, den })
}

fn reduce(mut num: u64, mut den: u64) -> Rational {
    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 {
            a
        } else {
            gcd(b, a % b)
        }
    }
    let g = gcd(num, den).max(1);
    num /= g;
    den /= g;
    while num > u64::from(u32::MAX) || den > u64::from(u32::MAX) {
        num /= 2;
        den /= 2;
    }
    Rational {
        num: num as u32,
        den: den.max(1) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_ntsc() {
        assert_eq!(
            rate_from_ns(41_708_333),
            Some(Rational {
                num: 24000,
                den: 1001
            })
        );
        assert_eq!(rate_from_ns(40_000_000), Some(Rational { num: 25, den: 1 }));
        assert_eq!(
            rate_from_ticks(48000, 2002),
            Some(Rational {
                num: 24000,
                den: 1001
            })
        );
        assert_eq!(rate_from_ticks(1000, 3), Some(Rational { num: 1000, den: 3 }));
        assert_eq!(rate_from_ticks(90000, 7), None); // 12857 fps: nonsense
    }

    #[test]
    fn reduces_like_ffmpeg() {
        let r = |n, d| sar(n, d).map(|r| (r.num, r.den));
        assert_eq!(r(480 * 853, 720 * 480), Some((853, 720)));
        assert_eq!(r(480 * 16, 720 * 9), Some((32, 27)));
        assert_eq!(r(0, 1), None);
        assert_eq!(r(5, 0), None);
        // Beyond `max`: the best continued-fraction approximation.
        assert_eq!(av_reduce(853, 720, 255), (186, 157));
        assert_eq!(av_reduce(355, 113, 100), (22, 7));
        assert_eq!(av_reduce(1, 3, 100), (1, 3));
    }

    /// Packs `(value, bits)` fields MSB-first into bytes.
    pub(crate) fn pack(fields: &[(u32, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        let (mut acc, mut n) = (0u64, 0u32);
        for &(v, bits) in fields {
            acc = (acc << bits) | u64::from(v);
            n += bits;
            while n >= 8 {
                out.push((acc >> (n - 8)) as u8);
                n -= 8;
            }
        }
        if n > 0 {
            out.push((acc << (8 - n)) as u8);
        }
        out
    }

    #[test]
    fn vui_aspect_table_and_extended() {
        use crate::bytes::Bits;
        let b = pack(&[(1, 1), (14, 8)]);
        assert_eq!(
            vui_aspect(&mut Bits::new(&b)),
            Some(Some(Rational { num: 4, den: 3 }))
        );
        let b = pack(&[(1, 1), (255, 8), (64, 16), (45, 16)]);
        assert_eq!(
            vui_aspect(&mut Bits::new(&b)),
            Some(Some(Rational { num: 64, den: 45 }))
        );
        let b = pack(&[(1, 1), (255, 8), (40, 16), (30, 16)]);
        assert_eq!(
            vui_aspect(&mut Bits::new(&b)),
            Some(Some(Rational { num: 4, den: 3 }))
        );
        let b = pack(&[(1, 1), (0, 8)]);
        assert_eq!(vui_aspect(&mut Bits::new(&b)), Some(None), "unspecified");
        let b = pack(&[(1, 1), (200, 8)]);
        assert_eq!(vui_aspect(&mut Bits::new(&b)), Some(None), "reserved");
        assert_eq!(vui_aspect(&mut Bits::new(&[0x00])), Some(None), "absent");
    }
}
