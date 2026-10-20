//! H.265/HEVC: `hvcC` record and SPS parsing (ITU-T H.265 §7.3.2.2, Annex E).

use crate::bits::{unescape_rbsp, BitReader};

use super::VuiColor;

/// One parameter-set array of an `hvcC` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HvccArray {
    pub completeness: bool,
    pub nal_type: u8,
    pub nalus: Vec<Vec<u8>>,
}

/// Parsed `HEVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §8.3.3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcConfig {
    pub profile_space: u8,
    pub tier: bool,
    pub profile_idc: u8,
    pub compat_flags: u32,
    pub constraint_flags: [u8; 6],
    pub level_idc: u8,
    pub bit_depth_luma: u8,
    pub length_size: u8,
    pub arrays: Vec<HvccArray>,
}

impl HevcConfig {
    pub fn parse(rec: &[u8]) -> Option<Self> {
        if rec.len() < 23 {
            return None;
        }
        let profile_space = rec[1] >> 6;
        let tier = rec[1] & 0x20 != 0;
        let profile_idc = rec[1] & 0x1F;
        let compat_flags = u32::from_be_bytes([rec[2], rec[3], rec[4], rec[5]]);
        let mut constraint_flags = [0u8; 6];
        constraint_flags.copy_from_slice(&rec[6..12]);
        let level_idc = rec[12];
        let bit_depth_luma = (rec[17] & 0x07) + 8;
        let length_size = (rec[21] & 0x03) + 1;
        let n_arrays = rec[22] as usize;
        let mut p = 23usize;
        let mut arrays = Vec::with_capacity(n_arrays);
        for _ in 0..n_arrays {
            let h = *rec.get(p)?;
            let n = u16::from_be_bytes([*rec.get(p + 1)?, *rec.get(p + 2)?]) as usize;
            p += 3;
            let mut nalus = Vec::with_capacity(n);
            for _ in 0..n {
                let len = u16::from_be_bytes([*rec.get(p)?, *rec.get(p + 1)?]) as usize;
                p += 2;
                nalus.push(rec.get(p..p + len)?.to_vec());
                p += len;
            }
            arrays.push(HvccArray {
                completeness: h & 0x80 != 0,
                nal_type: h & 0x3F,
                nalus,
            });
        }
        Some(HevcConfig {
            profile_space,
            tier,
            profile_idc,
            compat_flags,
            constraint_flags,
            level_idc,
            bit_depth_luma,
            length_size,
            arrays,
        })
    }

    /// NAL units of one type (32 VPS, 33 SPS, 34 PPS, 39 prefix SEI).
    pub fn nalus(&self, nal_type: u8) -> impl Iterator<Item = &[u8]> {
        self.arrays
            .iter()
            .filter(move |a| a.nal_type == nal_type)
            .flat_map(|a| a.nalus.iter().map(|n| n.as_slice()))
    }

    /// RFC 6381 / ISO 14496-15 Annex E codec string, e.g. `hvc1.2.4.L153.B0`.
    pub fn codec_string(&self, fourcc: &str) -> String {
        let space = match self.profile_space {
            1 => "A",
            2 => "B",
            3 => "C",
            _ => "",
        };
        // Compatibility flags are written in reverse bit order (Annex E.3).
        let compat = self.compat_flags.reverse_bits();
        let tier = if self.tier { 'H' } else { 'L' };
        let mut s = format!(
            "{fourcc}.{space}{}.{compat:X}.{tier}{}",
            self.profile_idc, self.level_idc
        );
        let last_nonzero = self.constraint_flags.iter().rposition(|&b| b != 0);
        if let Some(last) = last_nonzero {
            for b in &self.constraint_flags[..=last] {
                s.push_str(&format!(".{b:X}"));
            }
        }
        s
    }
}

/// The SPS facts the playback engine cares about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HevcSps {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    /// `sps_max_num_reorder_pics[HighestTid]`.
    pub max_num_reorder_pics: u32,
    pub color: Option<VuiColor>,
    /// VUI sample aspect ratio when not square.
    pub sar: Option<(u32, u32)>,
}

fn skip_profile_tier_level(r: &mut BitReader<'_>, max_sub_layers_minus1: u32) -> Option<()> {
    r.skip(88)?; // general profile_space..constraint flags
    r.skip(8)?; // general_level_idc
    let mut sub_profile = [false; 8];
    let mut sub_level = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_profile[i] = r.bit()?;
        sub_level[i] = r.bit()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.skip(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_profile[i] {
            r.skip(88)?;
        }
        if sub_level[i] {
            r.skip(8)?;
        }
    }
    Some(())
}

fn skip_scaling_list_data(r: &mut BitReader<'_>) -> Option<()> {
    for size_id in 0..4 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0;
        while matrix_id < 6 {
            if !r.bit()? {
                r.ue()?;
            } else {
                let coef_num = 64.min(1 << (4 + (size_id << 1)));
                if size_id > 1 {
                    r.se()?;
                }
                for _ in 0..coef_num {
                    r.se()?;
                }
            }
            matrix_id += step;
        }
    }
    Some(())
}

/// Parses `st_ref_pic_set(idx)` and returns NumDeltaPocs[idx].
fn parse_st_rps(r: &mut BitReader<'_>, idx: usize, num_delta_pocs: &[u32]) -> Option<u32> {
    let inter = idx != 0 && r.bit()?;
    if inter {
        // In the SPS, delta_idx_minus1 is not present (inferred 0).
        r.skip(1)?; // delta_rps_sign
        r.ue()?; // abs_delta_rps_minus1
        let ref_idx = idx - 1;
        let mut count = 0;
        for _ in 0..=num_delta_pocs[ref_idx] {
            let used = r.bit()?;
            let use_delta = if used { true } else { r.bit()? };
            if used || use_delta {
                count += 1;
            }
        }
        Some(count)
    } else {
        let neg = r.ue()?;
        let pos = r.ue()?;
        if neg > 16 || pos > 16 {
            return None;
        }
        for _ in 0..neg + pos {
            r.ue()?;
            r.skip(1)?;
        }
        Some(neg + pos)
    }
}

/// Parses an SPS NAL unit (including its 2-byte NAL header).
pub fn parse_sps(nal: &[u8]) -> Option<HevcSps> {
    if nal.len() < 4 || (nal[0] >> 1) & 0x3F != 33 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[2..]);
    let mut r = BitReader::new(&rbsp);
    r.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.u32(3)?;
    r.skip(1)?;
    skip_profile_tier_level(&mut r, max_sub_layers_minus1)?;
    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc == 3 {
        r.skip(1)?;
    }
    let mut width = r.ue()?;
    let mut height = r.ue()?;
    if r.bit()? {
        let (sub_w, sub_h) = match chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let (l, rr, t, b) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        width = width.saturating_sub(sub_w * (l + rr));
        height = height.saturating_sub(sub_h * (t + b));
    }
    let bit_depth = 8 + r.ue()?.min(8) as u8;
    r.ue()?; // bit_depth_chroma_minus8
    let log2_max_poc_lsb = r.ue()? + 4;
    let ordering_all = r.bit()?;
    let first = if ordering_all { 0 } else { max_sub_layers_minus1 };
    let mut reorder = 0;
    for _ in first..=max_sub_layers_minus1 {
        r.ue()?; // max_dec_pic_buffering_minus1
        reorder = r.ue()?; // keep the highest sub-layer's value
        r.ue()?; // max_latency_increase_plus1
    }
    let mut sps = HevcSps {
        width,
        height,
        bit_depth,
        max_num_reorder_pics: reorder,
        color: None,
        sar: None,
    };

    // Continue to the VUI for the colour description. Any failure from here
    // on keeps the facts already parsed.
    let mut sar = None;
    let vui = (|| -> Option<Option<VuiColor>> {
        for _ in 0..6 {
            r.ue()?; // block/transform sizes and hierarchy depths
        }
        if r.bit()? && r.bit()? {
            skip_scaling_list_data(&mut r)?;
        }
        r.skip(2)?; // amp, sao
        if r.bit()? {
            r.skip(8)?;
            r.ue()?;
            r.ue()?;
            r.skip(1)?;
        }
        let n_rps = r.ue()? as usize;
        if n_rps > 64 {
            return None;
        }
        let mut num_delta_pocs = Vec::with_capacity(n_rps);
        for i in 0..n_rps {
            let n = parse_st_rps(&mut r, i, &num_delta_pocs)?;
            num_delta_pocs.push(n);
        }
        if r.bit()? {
            let n = r.ue()?;
            for _ in 0..n.min(33) {
                r.skip(log2_max_poc_lsb as usize + 1)?;
            }
        }
        r.skip(2)?; // temporal mvp, strong intra smoothing
        if !r.bit()? {
            return Some(None);
        }
        if r.bit()? {
            let idc = r.u8(8)?;
            let ext = if idc == 255 {
                Some((r.u32(16)? as u16, r.u32(16)? as u16))
            } else {
                None
            };
            sar = super::vui_sar(idc, ext);
        }
        if r.bit()? {
            r.skip(1)?;
        }
        if r.bit()? {
            r.skip(3)?;
            let full_range = r.bit()?;
            if r.bit()? {
                return Some(Some(VuiColor {
                    primaries: r.u8(8)?,
                    transfer: r.u8(8)?,
                    matrix: r.u8(8)?,
                    full_range,
                }));
            }
        }
        Some(None)
    })();
    sps.color = vui.flatten();
    sps.sar = sar;
    Some(sps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_string_main10() {
        let c = HevcConfig {
            profile_space: 0,
            tier: false,
            profile_idc: 2,
            compat_flags: 0x2000_0000,
            constraint_flags: [0xB0, 0, 0, 0, 0, 0],
            level_idc: 153,
            bit_depth_luma: 10,
            length_size: 4,
            arrays: vec![],
        };
        assert_eq!(c.codec_string("hvc1"), "hvc1.2.4.L153.B0");
    }

    #[test]
    fn codec_string_main_high_tier() {
        let c = HevcConfig {
            profile_space: 0,
            tier: true,
            profile_idc: 1,
            compat_flags: 0x6000_0000,
            constraint_flags: [0x90, 0, 0, 0, 0, 0],
            level_idc: 120,
            bit_depth_luma: 8,
            length_size: 4,
            arrays: vec![],
        };
        assert_eq!(c.codec_string("hev1"), "hev1.1.6.H120.90");
    }
}
