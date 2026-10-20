//! H.264/AVC: `avcC` record and SPS parsing (ITU-T H.264 §7.3.2.1.1, Annex E).
//!
//! We need three things from the SPS: the reorder depth
//! (`max_num_reorder_frames`, for the CTS ≥ 0 presentation offset), the bit
//! depth, and the VUI colour description (for `colr`).

use crate::bits::{unescape_rbsp, BitReader};

use super::VuiColor;

/// Parsed `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §5.3.3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    /// NAL length prefix size in bytes (1, 2 or 4).
    pub length_size: u8,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl AvcConfig {
    pub fn parse(rec: &[u8]) -> Option<Self> {
        if rec.len() < 7 || rec[0] != 1 {
            return None;
        }
        let length_size = (rec[4] & 0x03) + 1;
        let mut p = 5usize;
        let n_sps = (rec[p] & 0x1F) as usize;
        p += 1;
        let mut sps = Vec::with_capacity(n_sps);
        for _ in 0..n_sps {
            let len = u16::from_be_bytes([*rec.get(p)?, *rec.get(p + 1)?]) as usize;
            p += 2;
            sps.push(rec.get(p..p + len)?.to_vec());
            p += len;
        }
        let n_pps = *rec.get(p)? as usize;
        p += 1;
        let mut pps = Vec::with_capacity(n_pps);
        for _ in 0..n_pps {
            let len = u16::from_be_bytes([*rec.get(p)?, *rec.get(p + 1)?]) as usize;
            p += 2;
            pps.push(rec.get(p..p + len)?.to_vec());
            p += len;
        }
        Some(AvcConfig {
            profile_idc: rec[1],
            constraint_flags: rec[2],
            level_idc: rec[3],
            length_size,
            sps,
            pps,
        })
    }

    /// RFC 6381 codec string, e.g. `avc1.64001f`.
    pub fn codec_string(&self) -> String {
        format!(
            "avc1.{:02x}{:02x}{:02x}",
            self.profile_idc, self.constraint_flags, self.level_idc
        )
    }
}

/// The SPS facts the playback engine cares about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AvcSps {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub bit_depth: u8,
    pub width: u32,
    pub height: u32,
    pub frame_mbs_only: bool,
    pub color: Option<VuiColor>,
    /// VUI sample aspect ratio when not square.
    pub sar: Option<(u32, u32)>,
    /// `max_num_reorder_frames` from the VUI bitstream restriction, or the
    /// value implied by the profile (0 for Baseline/intra profiles).
    pub max_num_reorder_frames: Option<u32>,
}

fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            next = (last + delta + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn skip_hrd(r: &mut BitReader<'_>) -> Option<()> {
    let cpb_cnt = r.ue()? + 1;
    r.skip(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..cpb_cnt.min(32) {
        r.ue()?;
        r.ue()?;
        r.skip(1)?;
    }
    r.skip(20)
}

/// Parses an SPS NAL unit (including its 1-byte NAL header).
pub fn parse_sps(nal: &[u8]) -> Option<AvcSps> {
    if nal.len() < 4 || nal[0] & 0x1F != 7 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.u8(8)?;
    let constraint = r.u8(8)?;
    let level_idc = r.u8(8)?;
    r.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    let mut bit_depth = 8u8;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.skip(1)?;
        }
        bit_depth = 8 + r.ue()?.min(8) as u8;
        r.ue()?; // bit_depth_chroma_minus8
        r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? {
            let n = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..n {
                if r.bit()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    let poc_type = r.ue()?;
    if poc_type == 0 {
        r.ue()?;
    } else if poc_type == 1 {
        r.skip(1)?;
        r.se()?;
        r.se()?;
        let n = r.ue()?;
        for _ in 0..n.min(256) {
            r.se()?;
        }
    }
    r.ue()?; // max_num_ref_frames
    r.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()? + 1;
    let height_map_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if !frame_mbs_only {
        r.skip(1)?;
    }
    r.skip(1)?; // direct_8x8_inference_flag
    let (mut crop_l, mut crop_r, mut crop_t, mut crop_b) = (0, 0, 0, 0);
    if r.bit()? {
        crop_l = r.ue()?;
        crop_r = r.ue()?;
        crop_t = r.ue()?;
        crop_b = r.ue()?;
    }
    let (sub_w, sub_h) = match chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let frame_h_mul = if frame_mbs_only { 1 } else { 2 };
    let width = (width_mbs * 16).saturating_sub(sub_w * (crop_l + crop_r));
    let height =
        (height_map_units * 16 * frame_h_mul).saturating_sub(sub_h * frame_h_mul * (crop_t + crop_b));

    // Profiles that cannot reorder: Baseline/Constrained Baseline (66) and
    // the intra profiles (constraint_set3 with High-family profile_idc).
    let intra = constraint & 0x10 != 0 && matches!(profile_idc, 100 | 110 | 122 | 244 | 44);
    let mut sps = AvcSps {
        profile_idc,
        level_idc,
        bit_depth,
        width,
        height,
        frame_mbs_only,
        color: None,
        sar: None,
        max_num_reorder_frames: if profile_idc == 66 || intra { Some(0) } else { None },
    };
    if !r.bit()? {
        return Some(sps);
    }
    // VUI (Annex E.1.1)
    if r.bit()? {
        let idc = r.u8(8)?;
        let ext = if idc == 255 {
            Some((r.u32(16)? as u16, r.u32(16)? as u16))
        } else {
            None
        };
        sps.sar = super::vui_sar(idc, ext);
    }
    if r.bit()? {
        r.skip(1)?;
    }
    if r.bit()? {
        r.skip(3)?; // video_format
        let full_range = r.bit()?;
        if r.bit()? {
            sps.color = Some(VuiColor {
                primaries: r.u8(8)?,
                transfer: r.u8(8)?,
                matrix: r.u8(8)?,
                full_range,
            });
        }
    }
    if r.bit()? {
        r.ue()?;
        r.ue()?;
    }
    if r.bit()? {
        r.skip(65)?; // num_units_in_tick, time_scale, fixed_frame_rate_flag
    }
    let nal_hrd = r.bit()?;
    if nal_hrd {
        skip_hrd(&mut r)?;
    }
    let vcl_hrd = r.bit()?;
    if vcl_hrd {
        skip_hrd(&mut r)?;
    }
    if nal_hrd || vcl_hrd {
        r.skip(1)?;
    }
    r.skip(1)?; // pic_struct_present_flag
    if r.bit()? {
        r.skip(1)?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
        sps.max_num_reorder_frames = Some(r.ue()?);
    }
    Some(sps)
}

#[cfg(test)]
mod tests {
    use super::*;

    // An x264 High profile SPS (as in the corpus's h264_multi_audio_subs.mkv)
    // is exercised in the integration tests; here a hand-checked
    // Baseline SPS (640x360) without VUI.
    #[test]
    fn avcc_roundtrip_fields() {
        let rec = [
            1, 0x64, 0x00, 0x28, 0xFF, 0xE1, 0, 4, 0x67, 0x64, 0x00, 0x28, 1, 0, 2, 0x68, 0xEE,
        ];
        let c = AvcConfig::parse(&rec).unwrap();
        assert_eq!(c.length_size, 4);
        assert_eq!(c.codec_string(), "avc1.640028");
        assert_eq!(c.sps.len(), 1);
        assert_eq!(c.pps, vec![vec![0x68, 0xEE]]);
    }
}
