//! HEVC: `hvcC` (ISO/IEC 14496-15 §8.3.3), SPS (ITU-T H.265 §7.3.2.2) and the
//! in-band NAL units we care about (SPS, prefix SEI, Dolby Vision RPU).

use flux_core::ColorInfo;

use super::{sei, VideoCfg};
use crate::bytes::{rbsp, Bits, Rd};

const NAL_SPS: u8 = 33;
const NAL_PREFIX_SEI: u8 = 39;
const NAL_SUFFIX_SEI: u8 = 40;
/// Dolby Vision RPU travels in `UNSPEC62`; the enhancement layer in `UNSPEC63`.
const NAL_DOVI_RPU: u8 = 62;

/// HEVC profile names as ffprobe prints them.
pub(crate) fn profile_name(idc: u8) -> &'static str {
    match idc {
        1 => "Main",
        2 => "Main 10",
        3 => "Main Still Picture",
        4 => "Rext",
        5 => "High Throughput",
        9 => "Screen Content Coding",
        _ => "Unknown",
    }
}

/// Parse an `HEVCDecoderConfigurationRecord` (MP4 `hvcC`, Matroska
/// `V_MPEGH/ISO/HEVC` CodecPrivate) including its SPS and SEI arrays.
/// x265 puts the HDR10 mastering/CLL SEIs in the record when
/// `repeat-headers`/`hdr10` are on, which saves the frame read entirely.
pub(crate) fn parse_hvcc(b: &[u8]) -> Option<VideoCfg> {
    let mut r = Rd::new(b);
    if r.u8()? != 1 {
        return None;
    }
    let p = r.u8()?;
    let tier_high = p & 0x20 != 0;
    let profile_idc = p & 0x1f;
    r.skip(4 + 6)?; // compatibility flags, constraint flags
    let level_idc = r.u8()?;
    r.skip(2 + 1)?; // min_spatial_segmentation, parallelismType
    let _chroma = r.u8()? & 3;
    let depth_luma = (r.u8()? & 7) + 8;
    r.skip(1 + 2)?; // depth chroma, avgFrameRate
    let len_size = usize::from(r.u8()? & 3) + 1;
    let num_arrays = r.u8()?;
    let mut cfg = VideoCfg {
        profile: Some(profile_label(profile_idc, tier_high)),
        level: Some(u16::from(level_idc)),
        bit_depth: Some(depth_luma),
        nal_len_size: Some(len_size),
        ..VideoCfg::default()
    };
    // Collect whatever complete NAL units the arrays hold; a truncated record
    // keeps the record-level profile/level/depth.
    let mut nals = Vec::new();
    'arrays: for _ in 0..num_arrays {
        // array_completeness + NAL type (we read the type from each NAL).
        let (Some(_), Some(n)) = (r.u8(), r.be16()) else {
            break;
        };
        for _ in 0..n {
            let Some(nal) = r.be16().and_then(|len| r.take(usize::from(len))) else {
                break 'arrays;
            };
            nals.push(nal);
        }
    }
    scan_nals(&nals, &mut cfg);
    Some(cfg)
}

/// ffprobe prints only the profile name; the tier is not part of it.
fn profile_label(idc: u8, _tier_high: bool) -> String {
    profile_name(idc).to_string()
}

/// Fold SPS / SEI / RPU NAL units (each starting with its 2-byte header) into `cfg`.
pub(crate) fn scan_nals(nals: &[&[u8]], cfg: &mut VideoCfg) {
    for nal in nals {
        let Some(&h0) = nal.first() else { continue };
        let ty = (h0 >> 1) & 0x3f;
        match ty {
            NAL_SPS if cfg.width.is_none() || cfg.color.is_none() => {
                if let Some(sps) = parse_sps(nal) {
                    cfg.absorb(sps);
                }
            }
            NAL_PREFIX_SEI | NAL_SUFFIX_SEI => {
                if let Some(body) = nal.get(2..) {
                    sei::parse_sei_rbsp(&rbsp(body), cfg);
                }
            }
            NAL_DOVI_RPU => cfg.dovi_rpu = true,
            _ => {}
        }
    }
}

/// Is this NAL a VCL (slice) unit? Frame scans stop there: SEI and parameter
/// sets precede the first slice of an access unit.
pub(crate) fn is_vcl(nal: &[u8]) -> bool {
    nal.first().is_some_and(|h| (h >> 1) & 0x3f < 32)
}

/// Parse a sequence parameter set NAL unit (including its 2-byte header).
pub(crate) fn parse_sps(nal: &[u8]) -> Option<VideoCfg> {
    if (nal.first()? >> 1) & 0x3f != NAL_SPS {
        return None;
    }
    let data = rbsp(nal.get(2..)?);
    let mut b = Bits::new(&data);
    b.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = b.u(3)?;
    b.skip(1)?; // temporal_id_nesting
    let (profile_idc, tier_high, level_idc) = profile_tier_level(&mut b, max_sub_layers_minus1)?;
    b.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = b.ue()?;
    if chroma_format_idc == 3 {
        b.flag()?; // separate_colour_plane_flag
    }
    let width = b.ue()?;
    let height = b.ue()?;
    let (mut cl, mut cr, mut ct, mut cb) = (0u32, 0u32, 0u32, 0u32);
    if b.flag()? {
        cl = b.ue()?;
        cr = b.ue()?;
        ct = b.ue()?;
        cb = b.ue()?;
    }
    let bit_depth = b.ue()? + 8;
    b.ue()?; // bit_depth_chroma_minus8
    let (sub_w, sub_h) = match chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let mut cfg = VideoCfg {
        profile: Some(profile_label(profile_idc, tier_high)),
        level: Some(u16::from(level_idc)),
        bit_depth: u8::try_from(bit_depth).ok(),
        width: Some(width.saturating_sub(cl.saturating_add(cr).saturating_mul(sub_w))),
        height: Some(height.saturating_sub(ct.saturating_add(cb).saturating_mul(sub_h))),
        interlaced: None,
        ..VideoCfg::default()
    };
    // Everything after this point only feeds the VUI; failures keep the basics.
    let _ = parse_sps_tail(&mut b, max_sub_layers_minus1, &mut cfg);
    Some(cfg)
}

fn parse_sps_tail(b: &mut Bits<'_>, max_sub_layers_minus1: u32, cfg: &mut VideoCfg) -> Option<()> {
    let log2_max_poc_lsb = b.ue()? + 4;
    let ordering_all = b.flag()?;
    let start = if ordering_all { 0 } else { max_sub_layers_minus1 };
    for _ in start..=max_sub_layers_minus1 {
        b.ue()?;
        b.ue()?;
        b.ue()?;
    }
    for _ in 0..6 {
        b.ue()?; // log2_min_luma_cb … max_transform_hierarchy_depth_intra
    }
    if b.flag()? && b.flag()? {
        // scaling_list_enabled_flag && sps_scaling_list_data_present_flag
        skip_scaling_list_data(b)?;
    }
    b.flag()?; // amp_enabled_flag
    b.flag()?; // sample_adaptive_offset_enabled_flag
    if b.flag()? {
        // pcm_enabled_flag
        b.skip(8)?;
        b.ue()?;
        b.ue()?;
        b.flag()?;
    }
    let num_st_rps = b.ue()?;
    if num_st_rps > 64 {
        return None;
    }
    let mut num_delta_pocs: Vec<u32> = Vec::with_capacity(num_st_rps as usize);
    for idx in 0..num_st_rps {
        let n = st_ref_pic_set(b, idx, &num_delta_pocs)?;
        num_delta_pocs.push(n);
    }
    if b.flag()? {
        // long_term_ref_pics_present_flag
        let n = b.ue()?;
        if n > 32 {
            return None;
        }
        for _ in 0..n {
            b.skip(log2_max_poc_lsb as usize)?;
            b.flag()?;
        }
    }
    b.flag()?; // sps_temporal_mvp_enabled_flag
    b.flag()?; // strong_intra_smoothing_enabled_flag
    if b.flag()? {
        parse_vui(b, cfg)?;
    }
    Some(())
}

/// `profile_tier_level(1, maxNumSubLayersMinus1)` (§7.3.3). Returns
/// (general_profile_idc, general_tier_flag, general_level_idc).
fn profile_tier_level(b: &mut Bits<'_>, max_sub_layers_minus1: u32) -> Option<(u8, bool, u8)> {
    b.skip(2)?; // general_profile_space
    let tier = b.flag()?;
    let profile_idc = b.u8(5)?;
    b.skip(32)?; // compatibility flags
    b.skip(4 + 43 + 1)?; // source flags, constraint flags, inbld/reserved
    let level_idc = b.u8(8)?;
    let mut sub_profile = [false; 8];
    let mut sub_level = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_profile[i] = b.flag()?;
        sub_level[i] = b.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            b.skip(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_profile[i] {
            b.skip(88)?;
        }
        if sub_level[i] {
            b.skip(8)?;
        }
    }
    Some((profile_idc, tier, level_idc))
}

fn skip_scaling_list_data(b: &mut Bits<'_>) -> Option<()> {
    for size_id in 0..4u32 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0;
        while matrix_id < 6 {
            if !b.flag()? {
                b.ue()?; // scaling_list_pred_matrix_id_delta
            } else {
                let coef_num = 64.min(1 << (4 + (size_id << 1)));
                if size_id > 1 {
                    b.se()?; // dc coef
                }
                for _ in 0..coef_num {
                    b.se()?;
                }
            }
            matrix_id += step;
        }
    }
    Some(())
}

/// `st_ref_pic_set(stRpsIdx)` (§7.3.7). Returns NumDeltaPocs for this set.
fn st_ref_pic_set(b: &mut Bits<'_>, idx: u32, prev: &[u32]) -> Option<u32> {
    let inter = idx != 0 && b.flag()?;
    if inter {
        // In the SPS, delta_idx_minus1 is absent and RefRpsIdx = stRpsIdx - 1.
        b.flag()?; // delta_rps_sign
        b.ue()?; // abs_delta_rps_minus1
        let ref_n = *prev.get(idx as usize - 1)?;
        let mut n = 0;
        for _ in 0..=ref_n {
            let used = b.flag()?;
            let use_delta = if used { true } else { b.flag()? };
            if use_delta {
                n += 1;
            }
        }
        Some(n)
    } else {
        let neg = b.ue()?;
        let pos = b.ue()?;
        if neg > 16 || pos > 16 {
            return None;
        }
        for _ in 0..(neg + pos) {
            b.ue()?;
            b.flag()?;
        }
        Some(neg + pos)
    }
}

/// VUI (Annex E.2.1) up to the timing info.
fn parse_vui(b: &mut Bits<'_>, cfg: &mut VideoCfg) -> Option<()> {
    cfg.sar = super::vui_aspect(b)?;
    if b.flag()? {
        b.flag()?;
    }
    if b.flag()? {
        b.skip(3)?;
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
    b.flag()?; // neutral_chroma_indication_flag
    let field_seq = b.flag()?;
    cfg.interlaced = Some(field_seq);
    b.flag()?; // frame_field_info_present_flag
    if b.flag()? {
        // default_display_window_flag
        for _ in 0..4 {
            b.ue()?;
        }
    }
    if b.flag()? {
        let num_units_in_tick = b.u(32)?;
        let time_scale = b.u(32)?;
        cfg.frame_rate = super::rate_from_ticks(u64::from(time_scale), u64::from(num_units_in_tick));
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_is_none() {
        assert!(parse_hvcc(&[1, 2, 3]).is_none());
        assert!(parse_sps(&[0x42, 0x01, 0xff]).is_none());
        let mut cfg = VideoCfg::default();
        scan_nals(&[&[0x4e, 0x01, 0xff, 0xff, 0xff]], &mut cfg); // truncated SEI: no panic
        assert!(cfg.mastering.is_none());
    }
}
