//! AV1: `av1C` (AV1-ISOBMFF §2.3), sequence header OBU (AV1 spec §5.5) and
//! metadata OBUs (§5.8: HDR CLL, HDR MDCV, ITU-T T.35).

use flux_core::{ColorInfo, ContentLightLevel, MasteringDisplay};

use super::{sei, VideoCfg};
use crate::bytes::{Bits, Rd};

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_METADATA: u8 = 5;

/// AV1 profile names as ffprobe prints them.
fn profile_name(p: u8) -> &'static str {
    match p {
        0 => "Main",
        1 => "High",
        2 => "Professional",
        _ => "Unknown",
    }
}

/// Parse an `AV1CodecConfigurationRecord` (MP4 `av1C`, Matroska `V_AV1`
/// CodecPrivate) and any configOBUs it carries.
pub(crate) fn parse_av1c(b: &[u8]) -> Option<VideoCfg> {
    let mut r = Rd::new(b);
    let marker_version = r.u8()?;
    if marker_version & 0x80 == 0 {
        return None;
    }
    let p = r.u8()?;
    let seq_profile = p >> 5;
    let seq_level_idx_0 = p & 0x1f;
    let f = r.u8()?;
    let high_bitdepth = f & 0x40 != 0;
    let twelve_bit = f & 0x20 != 0;
    r.u8()?; // initial_presentation_delay
    let bit_depth = if seq_profile == 2 && high_bitdepth {
        if twelve_bit {
            12
        } else {
            10
        }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let mut cfg = VideoCfg {
        profile: Some(profile_name(seq_profile).to_string()),
        level: Some(u16::from(seq_level_idx_0)),
        bit_depth: Some(bit_depth),
        ..VideoCfg::default()
    };
    scan_obus(r.rest(), &mut cfg);
    Some(cfg)
}

/// Walk low-overhead-format OBUs (each with `obu_has_size_field`) and fold the
/// sequence header and metadata OBUs into `cfg`. Stops at the first frame OBU.
pub(crate) fn scan_obus(data: &[u8], cfg: &mut VideoCfg) {
    let mut r = Rd::new(data);
    while r.remaining() > 0 {
        let Some(h) = r.u8() else { return };
        let ty = (h >> 3) & 0xf;
        if h & 0x04 != 0 && r.u8().is_none() {
            return; // extension header
        }
        let size = if h & 0x02 != 0 {
            match r.leb128().and_then(|s| usize::try_from(s).ok()) {
                Some(s) => s,
                None => return,
            }
        } else {
            r.remaining()
        };
        let Some(payload) = r.take(size) else { return };
        match ty {
            OBU_SEQUENCE_HEADER => {
                if let Some(seq) = parse_sequence_header(payload) {
                    cfg.absorb(seq);
                }
            }
            OBU_METADATA => parse_metadata(payload, cfg),
            // Frame header / frame / tile group: nothing more to learn.
            3 | 4 | 6 => return,
            _ => {}
        }
    }
}

fn parse_metadata(p: &[u8], cfg: &mut VideoCfg) {
    let mut r = Rd::new(p);
    let Some(ty) = r.leb128() else { return };
    match ty {
        1 => {
            if let (Some(max_cll), Some(max_fall)) = (r.be16(), r.be16()) {
                cfg.cll = Some(ContentLightLevel { max_cll, max_fall });
            }
        }
        2 => {
            if let Some(m) = mdcv(&mut r) {
                cfg.mastering = Some(m);
            }
        }
        4 => {
            let rest = r.rest();
            if sei::is_hdr10plus_t35(rest) {
                cfg.hdr10plus = true;
            } else if sei::is_dovi_t35(rest) {
                cfg.dovi_rpu = true;
            }
        }
        _ => {}
    }
}

/// AV1 MDCV uses R, G, B order and different fixed-point units; convert to
/// the HEVC SEI convention used by `flux_core::MasteringDisplay`.
fn mdcv(r: &mut Rd<'_>) -> Option<MasteringDisplay> {
    // 0.16 fixed point → 0.00002 units.
    let chroma = |v: u16| ((u64::from(v) * 50_000 + 32_768) / 65_536) as u16;
    let mut rgb = [[0u16; 2]; 3];
    for c in &mut rgb {
        *c = [chroma(r.be16()?), chroma(r.be16()?)];
    }
    let white_point = [chroma(r.be16()?), chroma(r.be16()?)];
    // 24.8 and 18.14 fixed point cd/m² → 0.0001 cd/m².
    let max_luminance = ((u64::from(r.be32()?) * 10_000 + 128) / 256) as u32;
    let min_luminance = ((u64::from(r.be32()?) * 10_000 + 8192) / 16_384) as u32;
    Some(MasteringDisplay {
        primaries: [rgb[1], rgb[2], rgb[0]],
        white_point,
        max_luminance,
        min_luminance,
    })
}

/// `sequence_header_obu()` up to `color_config()`.
pub(crate) fn parse_sequence_header(p: &[u8]) -> Option<VideoCfg> {
    let mut b = Bits::new(p);
    let seq_profile = b.u8(3)?;
    b.flag()?; // still_picture
    let reduced = b.flag()?;
    let mut level = 0u8;
    let mut frame_rate = None;
    if reduced {
        level = b.u8(5)?;
    } else {
        let mut decoder_model_info_present = false;
        let mut buffer_delay_len = 0u32;
        if b.flag()? {
            // timing_info()
            let num_units_in_display_tick = b.u(32)?;
            let time_scale = b.u(32)?;
            let equal_picture_interval = b.flag()?;
            if equal_picture_interval {
                let ticks = u64::from(b.uvlc()?) + 1;
                frame_rate = super::rate_from_ticks(
                    u64::from(time_scale),
                    u64::from(num_units_in_display_tick) * ticks,
                );
            }
            decoder_model_info_present = b.flag()?;
            if decoder_model_info_present {
                buffer_delay_len = b.u(5)? + 1;
                b.skip(32)?; // num_units_in_decoding_tick
                b.skip(5 + 5)?;
            }
        }
        let initial_display_delay_present = b.flag()?;
        let op_cnt = b.u(5)? + 1;
        for i in 0..op_cnt {
            b.skip(12)?; // operating_point_idc
            let lvl = b.u8(5)?;
            if i == 0 {
                level = lvl;
            }
            if lvl > 7 {
                b.flag()?; // seq_tier
            }
            if decoder_model_info_present && b.flag()? {
                b.skip(2 * buffer_delay_len as usize + 1)?;
            }
            if initial_display_delay_present && b.flag()? {
                b.skip(4)?;
            }
        }
    }
    let wbits = b.u(4)? + 1;
    let hbits = b.u(4)? + 1;
    let width = b.u(wbits)?.checked_add(1)?;
    let height = b.u(hbits)?.checked_add(1)?;
    if !reduced && b.flag()? {
        b.skip(4 + 3)?; // frame id lengths
    }
    b.skip(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !reduced {
        b.skip(4)?; // interintra, masked, warped, dual_filter
        let order_hint = b.flag()?;
        if order_hint {
            b.skip(2)?; // jnt_comp, ref_frame_mvs
        }
        let force_sct = if b.flag()? { 2 } else { b.u(1)? };
        if force_sct > 0 && !b.flag()? {
            b.skip(1)?; // seq_force_integer_mv
        }
        if order_hint {
            b.skip(3)?;
        }
    }
    b.skip(3)?; // superres, cdef, restoration
                // color_config()
    let high_bitdepth = b.flag()?;
    let bit_depth = if seq_profile == 2 && high_bitdepth {
        if b.flag()? {
            12
        } else {
            10
        }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let mono = if seq_profile == 1 { false } else { b.flag()? };
    let mut color = ColorInfo::default();
    let (mut cp, mut tc, mut mc) = (2u8, 2u8, 2u8);
    if b.flag()? {
        cp = b.u8(8)?;
        tc = b.u8(8)?;
        mc = b.u8(8)?;
        color.primaries = Some(cp);
        color.transfer = Some(tc);
        color.matrix = Some(mc);
    }
    if mono {
        color.full_range = Some(b.flag()?);
    } else if cp == 1 && tc == 13 && mc == 0 {
        color.full_range = Some(true); // sRGB
    } else {
        color.full_range = Some(b.flag()?);
    }
    Some(VideoCfg {
        profile: Some(profile_name(seq_profile).to_string()),
        level: Some(u16::from(level)),
        bit_depth: Some(bit_depth),
        width: Some(width),
        height: Some(height),
        frame_rate,
        color: Some(color),
        ..VideoCfg::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdcv_units() {
        // R(0.68,0.32) G(0.265,0.69) B(0.15,0.06) WP(0.3127,0.329), 1000/0.0001 nits.
        let mut p = vec![2u8];
        for v in [0.68f64, 0.32, 0.265, 0.69, 0.15, 0.06, 0.3127, 0.329] {
            p.extend_from_slice(&((v * 65536.0).round() as u16).to_be_bytes());
        }
        p.extend_from_slice(&(1000u32 * 256).to_be_bytes());
        p.extend_from_slice(&((0.0001f64 * 16384.0).round() as u32).to_be_bytes());
        let mut cfg = VideoCfg::default();
        parse_metadata(&p, &mut cfg);
        let m = cfg.mastering.unwrap();
        assert_eq!(m.primaries[2], [34000, 16000]);
        assert_eq!(m.primaries[0], [13250, 34500]);
        assert_eq!(m.max_luminance, 10_000_000);
    }

    #[test]
    fn garbage_is_none() {
        assert!(parse_av1c(&[0x81]).is_none());
        let mut cfg = VideoCfg::default();
        scan_obus(&[0x0a, 0xff, 0xff], &mut cfg);
        assert_eq!(cfg, VideoCfg::default());
    }
}
