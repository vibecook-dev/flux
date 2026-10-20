//! MPEG-2 video sequence header + sequence extension (ITU-T H.262 §6.2.2).

use flux_core::Rational;

use super::VideoCfg;
use crate::bytes::Bits;

/// Find and parse the first `sequence_header` (and its extension) in an
/// MPEG-1/2 elementary stream fragment.
pub(crate) fn parse_mpeg2_es(es: &[u8]) -> Option<VideoCfg> {
    let pos = es.windows(4).position(|w| w == [0, 0, 1, 0xb3])?;
    let mut b = Bits::new(es.get(pos + 4..)?);
    let w = b.u(12)?;
    let h = b.u(12)?;
    let aspect_code = b.u(4)?;
    let rate_code = b.u(4)?;
    let mut cfg = VideoCfg {
        width: Some(w),
        height: Some(h),
        bit_depth: Some(8),
        frame_rate: frame_rate(rate_code),
        ..VideoCfg::default()
    };
    // sequence_extension: 00 00 01 B5, extension_start_code_identifier = 1.
    let rest = es.get(pos + 4..)?;
    if let Some(ep) = rest
        .windows(5)
        .position(|w| w[..4] == [0, 0, 1, 0xb5] && w[4] >> 4 == 1)
    {
        let mut e = Bits::new(rest.get(ep + 4..)?);
        e.skip(4)?;
        let pl = e.u8(8)?;
        let progressive = e.flag()?;
        e.skip(2)?; // chroma_format
        let hx = e.u(2)?;
        let vx = e.u(2)?;
        cfg.width = Some(w | (hx << 12));
        cfg.height = Some(h | (vx << 12));
        cfg.interlaced = Some(!progressive);
        cfg.profile = Some(
            match (pl >> 4) & 7 {
                1 => "High",
                2 => "Spatially Scalable",
                3 => "SNR Scalable",
                4 => "Main",
                5 => "Simple",
                _ => "Unknown",
            }
            .to_string(),
        );
        cfg.level = Some(u16::from(pl & 0xf));
        // sequence_display_extension (identifier 2), if any: pan-scan size.
        let display = rest
            .windows(5)
            .position(|w| w[..4] == [0, 0, 1, 0xb5] && w[4] >> 4 == 2)
            .and_then(|dp| display_size(rest.get(dp + 4..)?));
        if let (Some(w), Some(h)) = (cfg.width, cfg.height) {
            cfg.sar = mpeg2_sar(aspect_code, w, h, display);
        }
    } else if aspect_code == 1 {
        // MPEG-1: code 1 is square pixels; the other pel-aspect codes are
        // approximations ffmpeg derives with `av_d2q`, not handled here.
        cfg.sar = Some(Rational { num: 1, den: 1 });
    }
    Some(cfg)
}

/// `sequence_display_extension` → (display_horizontal_size, display_vertical_size).
fn display_size(ext: &[u8]) -> Option<(u32, u32)> {
    let mut e = Bits::new(ext);
    e.skip(4)?; // extension_start_code_identifier
    e.skip(3)?; // video_format
    if e.flag()? {
        e.skip(24)?; // colour_primaries, transfer_characteristics, matrix_coefficients
    }
    let w = e.u(14)?;
    e.skip(1)?; // marker
    let h = e.u(14)?;
    Some((w, h))
}

/// MPEG-2 SAR from `aspect_ratio_information` (a display aspect ratio for
/// codes 2–4), exactly as FFmpeg's `mpeg_decode_postinit` derives it, so it
/// matches ffprobe: the pan-scan size (sequence display extension) is used
/// only when it yields a 4:3 or 16:9 picture.
fn mpeg2_sar(code: u32, w: u32, h: u32, display: Option<(u32, u32)>) -> Option<Rational> {
    let (an, ad): (u64, u64) = match code {
        1 => return super::sar(1, 1),
        2 => (4, 3),
        3 => (16, 9),
        4 => (221, 100),
        _ => return None,
    };
    let (w, h) = (u64::from(w), u64::from(h));
    // FFmpeg keeps the pan-scan size in 1/16 pixel units.
    let pan = display.map(|(dw, dh)| (16 * u64::from(dw), 16 * u64::from(dh)));
    let use_pan = match pan {
        Some((pw, ph)) if pw > 0 && ph > 0 && w > 0 && h > 0 => {
            // dar = aspect / (pw/ph) * (w/h), compared exactly with 4:3 and 16:9.
            let (dn, dd) = (u128::from(an * ph * w), u128::from(ad * pw * h));
            dn * 3 == dd * 4 || dn * 9 == dd * 16
        }
        _ => false,
    };
    match (use_pan, pan) {
        (true, Some((pw, ph))) => super::sar(an * ph, ad * pw),
        _ => super::sar(an * h, ad * w),
    }
}

fn frame_rate(code: u32) -> Option<Rational> {
    let (num, den) = match code {
        1 => (24000, 1001),
        2 => (24, 1),
        3 => (25, 1),
        4 => (30000, 1001),
        5 => (30, 1),
        6 => (50, 1),
        7 => (60000, 1001),
        8 => (60, 1),
        _ => return None,
    };
    Some(Rational { num, den })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_header_1080() {
        // 1920x1080, aspect 3, 29.97 + extension Main@High progressive 4:2:0
        let es = [
            0, 0, 1, 0xb3, 0x78, 0x04, 0x38, 0x34, 0xff, 0xff, 0xe0, 0x18, 0, 0, 1, 0xb5, 0x14, 0x4a, 0x00,
            0x01,
        ];
        let c = parse_mpeg2_es(&es).unwrap();
        assert_eq!((c.width, c.height), (Some(1920), Some(1080)));
        assert_eq!(
            c.frame_rate,
            Some(Rational {
                num: 30000,
                den: 1001
            })
        );
        assert_eq!(c.profile.as_deref(), Some("Main"));
        assert_eq!(c.level, Some(4));
        assert_eq!(c.interlaced, Some(false));
        // aspect code 3 (16:9 DAR) at 1920x1080: square pixels.
        assert_eq!(c.sar, Some(Rational { num: 1, den: 1 }));
    }

    #[test]
    fn mpeg2_anamorphic() {
        let r = |code, w, h, d| mpeg2_sar(code, w, h, d).map(|r| (r.num, r.den));
        assert_eq!(r(3, 720, 480, None), Some((32, 27)));
        assert_eq!(r(2, 720, 576, None), Some((16, 15)));
        assert_eq!(r(3, 720, 576, None), Some((64, 45)));
        // A 4:3-flagged frame whose 540-wide pan-scan window makes an exact
        // 16:9 picture: FFmpeg derives the SAR from the window.
        assert_eq!(r(2, 720, 480, Some((540, 480))), Some((32, 27)));
        assert_eq!(r(2, 720, 480, None), Some((8, 9)));
        // A window that makes neither 4:3 nor 16:9 is ignored.
        assert_eq!(r(3, 720, 480, Some((704, 480))), Some((32, 27)));
        assert_eq!(r(0, 720, 480, None), None);
        assert_eq!(r(9, 720, 480, None), None);
    }
}
