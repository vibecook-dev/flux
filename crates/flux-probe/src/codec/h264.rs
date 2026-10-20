//! H.264 / AVC: `avcC` (ISO/IEC 14496-15 §5.3.3) and SPS (ITU-T H.264 §7.3.2.1).

use flux_core::ColorInfo;

use super::VideoCfg;
use crate::bytes::{rbsp, Bits, Rd};

/// Parse an `AVCDecoderConfigurationRecord` (MP4 `avcC`, Matroska
/// `V_MPEG4/ISO/AVC` CodecPrivate), including its first SPS.
pub(crate) fn parse_avcc(b: &[u8]) -> Option<VideoCfg> {
    let mut r = Rd::new(b);
    let version = r.u8()?;
    if version != 1 {
        return None;
    }
    let profile_idc = r.u8()?;
    let compat = r.u8()?;
    let level_idc = r.u8()?;
    let len_size = usize::from(r.u8()? & 3) + 1;
    let num_sps = r.u8()? & 0x1f;
    let mut cfg = VideoCfg {
        profile: Some(profile_name(profile_idc, compat).to_string()),
        level: Some(u16::from(level_idc)),
        nal_len_size: Some(len_size),
        ..VideoCfg::default()
    };
    // A truncated parameter set keeps the record-level profile/level.
    for _ in 0..num_sps {
        let Some(nal) = r.be16().and_then(|len| r.take(usize::from(len))) else {
            break;
        };
        if let Some(sps) = parse_sps(nal) {
            cfg.absorb(sps);
            break;
        }
    }
    Some(cfg)
}

/// Human profile name as ffprobe prints it.
pub(crate) fn profile_name(idc: u8, constraints: u8) -> &'static str {
    let c1 = constraints & 0x40 != 0;
    let c3 = constraints & 0x10 != 0;
    match idc {
        66 if c1 => "Constrained Baseline",
        66 => "Baseline",
        77 => "Main",
        88 => "Extended",
        100 => "High",
        110 if c3 => "High 10 Intra",
        110 => "High 10",
        122 if c3 => "High 4:2:2 Intra",
        122 => "High 4:2:2",
        244 if c3 => "High 4:4:4 Intra",
        244 => "High 4:4:4 Predictive",
        44 => "CAVLC 4:4:4",
        118 | 128 => "Multiview High",
        _ => "Unknown",
    }
}

/// Parse a sequence parameter set NAL unit (including its 1-byte header).
pub(crate) fn parse_sps(nal: &[u8]) -> Option<VideoCfg> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let data = rbsp(nal.get(1..)?);
    let mut b = Bits::new(&data);
    let profile_idc = b.u8(8)?;
    let constraints = b.u8(8)?;
    let level_idc = b.u8(8)?;
    b.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1u32;
    let mut separate_colour_plane = false;
    let mut bit_depth = 8u32;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = b.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = b.flag()?;
        }
        bit_depth = b.ue()? + 8;
        b.ue()?; // bit_depth_chroma_minus8
        b.flag()?; // qpprime_y_zero_transform_bypass_flag
        if b.flag()? {
            // seq_scaling_matrix_present_flag
            let n = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..n {
                if b.flag()? {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => {
            b.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            b.flag()?;
            b.se()?;
            b.se()?;
            let n = b.ue()?;
            if n > 255 {
                return None;
            }
            for _ in 0..n {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?; // max_num_ref_frames
    b.flag()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = u64::from(b.ue()?) + 1;
    let height_map_units = u64::from(b.ue()?) + 1;
    let frame_mbs_only = b.flag()?;
    if !frame_mbs_only {
        b.flag()?; // mb_adaptive_frame_field_flag
    }
    b.flag()?; // direct_8x8_inference_flag
    let (mut cl, mut cr, mut ct, mut cb) = (0u64, 0u64, 0u64, 0u64);
    if b.flag()? {
        cl = u64::from(b.ue()?);
        cr = u64::from(b.ue()?);
        ct = u64::from(b.ue()?);
        cb = u64::from(b.ue()?);
    }
    // §7.4.2.1.1 frame cropping units.
    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let field_factor = if frame_mbs_only { 1 } else { 2 };
    let (crop_x, crop_y) = match chroma_array_type {
        0 => (1, field_factor),
        1 => (2, 2 * field_factor),
        2 => (2, field_factor),
        _ => (1, field_factor),
    };
    let width = (width_mbs * 16).saturating_sub(crop_x * (cl + cr));
    let height = (height_map_units * 16 * field_factor).saturating_sub(crop_y * (ct + cb));

    let mut cfg = VideoCfg {
        profile: Some(profile_name(profile_idc, constraints).to_string()),
        level: Some(u16::from(level_idc)),
        bit_depth: u8::try_from(bit_depth).ok(),
        width: u32::try_from(width).ok(),
        height: u32::try_from(height).ok(),
        interlaced: Some(!frame_mbs_only),
        ..VideoCfg::default()
    };
    if b.flag().unwrap_or(false) {
        // VUI is best effort: a short buffer keeps what we already have.
        let _ = parse_vui(&mut b, &mut cfg);
    }
    Some(cfg)
}

/// Fold SPS / SEI NAL units (each starting with its 1-byte header) into `cfg`.
pub(crate) fn scan_nals(nals: &[&[u8]], cfg: &mut VideoCfg) {
    for nal in nals {
        match nal.first().map(|h| h & 0x1f) {
            Some(7) if cfg.width.is_none() || cfg.color.is_none() => {
                if let Some(sps) = parse_sps(nal) {
                    cfg.absorb(sps);
                }
            }
            Some(6) => {
                if let Some(body) = nal.get(1..) {
                    super::sei::parse_sei_rbsp(&rbsp(body), cfg);
                }
            }
            _ => {}
        }
    }
}

/// Is this NAL a VCL (slice) unit (types 1–5)?
pub(crate) fn is_vcl(nal: &[u8]) -> bool {
    nal.first().is_some_and(|h| (1..=5).contains(&(h & 0x1f)))
}

fn skip_scaling_list(b: &mut Bits<'_>, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            let delta = b.se()?;
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

/// VUI (Annex E.1.1) up to the timing info.
fn parse_vui(b: &mut Bits<'_>, cfg: &mut VideoCfg) -> Option<()> {
    cfg.sar = super::vui_aspect(b)?;
    if b.flag()? {
        b.flag()?; // overscan_appropriate_flag
    }
    if b.flag()? {
        // video_signal_type_present_flag
        b.skip(3)?; // video_format
        let full_range = b.flag()?;
        let mut c = ColorInfo {
            full_range: Some(full_range),
            ..ColorInfo::default()
        };
        if b.flag()? {
            c.primaries = Some(b.u8(8)?);
            c.transfer = Some(b.u8(8)?);
            c.matrix = Some(b.u8(8)?);
        }
        cfg.color = Some(c);
    }
    if b.flag()? {
        b.ue()?;
        b.ue()?;
    }
    if b.flag()? {
        // timing_info_present_flag; H.264 ticks are per field, hence ×2.
        let num_units_in_tick = b.u(32)?;
        let time_scale = b.u(32)?;
        cfg.frame_rate = super::rate_from_ticks(u64::from(time_scale), 2 * u64::from(num_units_in_tick));
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x264_1080p_sps() {
        // CodecPrivate of features/h264_multi_audio_subs.mkv (x264, High@4.0,
        // 1920x1088 coded and cropped to 1080, VUI timing 24000/1001).
        let avcc = [
            0x01, 0x64, 0x00, 0x28, 0xff, 0xe1, 0x00, 0x19, 0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78,
            0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x0f, 0xa4, 0x00, 0x02, 0xee, 0x00, 0x3c, 0x60, 0xc6,
            0x58, 0x01, 0x00, 0x04, 0x68, 0xef, 0xbc, 0xb0, 0xfd, 0xf8, 0xf8, 0x00,
        ];
        let cfg = parse_avcc(&avcc).expect("avcC");
        assert_eq!(cfg.profile.as_deref(), Some("High"));
        assert_eq!(cfg.level, Some(40));
        assert_eq!((cfg.width, cfg.height), (Some(1920), Some(1080)));
        assert_eq!(cfg.bit_depth, Some(8));
        assert_eq!(cfg.nal_len_size, Some(4));
        assert_eq!(cfg.interlaced, Some(false));
        assert_eq!(cfg.frame_rate.map(|r| (r.num, r.den)), Some((24000, 1001)));
        // Truncated SPS: keeps nothing rather than panicking.
        assert!(parse_avcc(&avcc[..12]).is_some_and(|c| c.width.is_none()));
    }

    #[test]
    fn garbage_is_none() {
        assert!(parse_avcc(&[1, 2, 3]).is_none());
        assert!(parse_sps(&[0x67]).is_none());
        assert!(parse_sps(&[0x67, 0xff, 0xff, 0xff, 0xff]).is_none());
    }
}
