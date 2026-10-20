//! Codec bitstream helpers: parameter sets, sample-entry configuration records,
//! HDR SEI, codec strings and per-frame durations.

pub mod audio;
pub mod h264;
pub mod hevc;
pub mod wrap;

use flux_core::{ContentLightLevel, MasteringDisplay};

use crate::bits::{length_prefixed_nals, unescape_rbsp};

/// Colour description from an SPS VUI (H.273 code points).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VuiColor {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

/// Sample aspect ratio of a VUI `aspect_ratio_idc` (H.264 Table E-1, the
/// same table in HEVC); `None` for unspecified, square (1) or reserved.
pub(crate) fn vui_sar(idc: u8, extended: Option<(u16, u16)>) -> Option<(u32, u32)> {
    const TABLE: [(u32, u32); 17] = [
        (0, 0),
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
    let (n, d) = match idc {
        255 => extended.map(|(w, h)| (w as u32, h as u32))?,
        i => *TABLE.get(i as usize)?,
    };
    (n > 0 && d > 0 && n != d).then_some((n, d))
}

/// HDR static metadata found in SEI messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeiHdr {
    pub mastering: Option<MasteringDisplay>,
    pub cll: Option<ContentLightLevel>,
}

impl SeiHdr {
    fn merge(&mut self, other: SeiHdr) {
        self.mastering = self.mastering.or(other.mastering);
        self.cll = self.cll.or(other.cll);
    }
}

/// Parses SEI payloads 137 (mastering display colour volume) and 144 (content
/// light level) from one SEI NAL unit. `header_len` is 1 for H.264, 2 for HEVC.
pub fn parse_hdr_sei(nal: &[u8], header_len: usize) -> SeiHdr {
    let mut out = SeiHdr::default();
    let Some(body) = nal.get(header_len..) else {
        return out;
    };
    let rbsp = unescape_rbsp(body);
    let mut p = 0usize;
    while p + 2 <= rbsp.len() && rbsp[p] != 0x80 {
        let mut ty = 0usize;
        while rbsp.get(p) == Some(&0xFF) {
            ty += 255;
            p += 1;
        }
        let Some(&b) = rbsp.get(p) else { break };
        ty += b as usize;
        p += 1;
        let mut size = 0usize;
        while rbsp.get(p) == Some(&0xFF) {
            size += 255;
            p += 1;
        }
        let Some(&b) = rbsp.get(p) else { break };
        size += b as usize;
        p += 1;
        let Some(payload) = rbsp.get(p..p + size) else {
            break;
        };
        let u16_at = |i: usize| u16::from_be_bytes([payload[i], payload[i + 1]]);
        let u32_at =
            |i: usize| u32::from_be_bytes([payload[i], payload[i + 1], payload[i + 2], payload[i + 3]]);
        match ty {
            137 if size >= 24 => {
                out.mastering = Some(MasteringDisplay {
                    primaries: [
                        [u16_at(0), u16_at(2)],
                        [u16_at(4), u16_at(6)],
                        [u16_at(8), u16_at(10)],
                    ],
                    white_point: [u16_at(12), u16_at(14)],
                    max_luminance: u32_at(16),
                    min_luminance: u32_at(20),
                });
            }
            144 if size >= 4 => {
                out.cll = Some(ContentLightLevel {
                    max_cll: u16_at(0),
                    max_fall: u16_at(2),
                });
            }
            _ => {}
        }
        p += size;
    }
    out
}

/// Video bitstream flavour, for NAL-level operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalFlavor {
    H264,
    Hevc,
}

impl NalFlavor {
    fn nal_type(self, nal: &[u8]) -> u8 {
        match self {
            NalFlavor::H264 => nal.first().map(|b| b & 0x1F).unwrap_or(0),
            NalFlavor::Hevc => nal.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0),
        }
    }

    fn is_parameter_set(self, t: u8) -> bool {
        match self {
            NalFlavor::H264 => matches!(t, 7 | 8 | 13),
            NalFlavor::Hevc => matches!(t, 32..=34),
        }
    }

    fn is_sei(self, t: u8) -> bool {
        match self {
            NalFlavor::H264 => t == 6,
            NalFlavor::Hevc => t == 39 || t == 40,
        }
    }

    fn header_len(self) -> usize {
        match self {
            NalFlavor::H264 => 1,
            NalFlavor::Hevc => 2,
        }
    }
}

/// True when a length-prefixed sample carries in-band parameter sets
/// (VPS/SPS/PPS), which `avc1`/`hvc1` sample entries do not allow.
pub fn has_inband_parameter_sets(sample: &[u8], length_size: usize, flavor: NalFlavor) -> bool {
    length_prefixed_nals(sample, length_size).any(|(_, nal)| flavor.is_parameter_set(flavor.nal_type(nal)))
}

/// Copies a length-prefixed sample without its parameter-set NAL units.
pub fn strip_parameter_sets(sample: &[u8], length_size: usize, flavor: NalFlavor) -> Vec<u8> {
    let mut out = Vec::with_capacity(sample.len());
    for (start, nal) in length_prefixed_nals(sample, length_size) {
        if !flavor.is_parameter_set(flavor.nal_type(nal)) {
            out.extend_from_slice(&sample[start..start + length_size + nal.len()]);
        }
    }
    out
}

/// What kind of random-access point a keyframe sample is: `Some(true)` for an
/// IDR picture (H.264 type 5, HEVC IDR_W_RADL/IDR_N_LP), `Some(false)` for an
/// open-GOP one (H.264 non-IDR I picture, HEVC CRA/BLA), `None` when the
/// sample holds no coded slice.
///
/// Firefox on macOS treats only IDR pictures as MSE random-access points
/// (VideoToolbox rejects a CRA as the first picture after a seek), so a
/// segment that starts on a CRA cannot be seeked into there.
pub fn idr_random_access(sample: &[u8], length_size: usize, flavor: NalFlavor) -> Option<bool> {
    // Only NAL headers are needed, so a sample truncated inside its first
    // slice (callers read a prefix) still classifies.
    let mut pos = 0usize;
    while length_size > 0 && pos + length_size < sample.len() {
        let n = sample[pos..pos + length_size]
            .iter()
            .fold(0usize, |n, &b| (n << 8) | b as usize);
        let start = pos + length_size;
        pos = start.saturating_add(n);
        let Some(nal) = sample.get(start..pos.min(sample.len())) else {
            break;
        };
        if nal.is_empty() {
            continue;
        }
        let t = flavor.nal_type(nal);
        match flavor {
            NalFlavor::H264 => match t {
                5 => return Some(true),
                1..=4 => return Some(false),
                _ => {}
            },
            NalFlavor::Hevc => match t {
                19 | 20 => return Some(true),
                0..=18 | 21 => return Some(false),
                _ => {}
            },
        }
    }
    None
}

/// Rewrites HEVC CRA NAL units (type 21) of a length-prefixed sample to
/// BLA_W_LP (type 16) in place: a BLA picture starts a new coded video
/// sequence (POC MSB reset, leading pictures dropped) and has the same slice
/// header syntax, so only the NAL header byte changes.
pub fn cra_to_bla(sample: &mut [u8], length_size: usize) {
    let mut pos = 0usize;
    while length_size > 0 && pos + length_size < sample.len() {
        let n = sample[pos..pos + length_size]
            .iter()
            .fold(0usize, |n, &b| (n << 8) | b as usize);
        let start = pos + length_size;
        if let Some(h) = sample.get_mut(start) {
            if (*h >> 1) & 0x3F == 21 {
                *h = (*h & 0x81) | (16 << 1);
            }
        }
        pos = start.saturating_add(n);
    }
}

/// HDR SEI from the NAL units of one (key)frame sample.
pub fn sample_hdr_sei(sample: &[u8], length_size: usize, flavor: NalFlavor) -> SeiHdr {
    let mut out = SeiHdr::default();
    for (_, nal) in length_prefixed_nals(sample, length_size) {
        if flavor.is_sei(flavor.nal_type(nal)) {
            out.merge(parse_hdr_sei(nal, flavor.header_len()));
        }
    }
    out
}

/// RFC 6381 codec string for AV1 from an `av1C` record: `av01.P.LLT.DD`.
pub fn av1_codec_string(av1c: &[u8]) -> Option<String> {
    if av1c.len() < 4 || av1c[0] & 0x80 == 0 {
        return None;
    }
    let profile = av1c[1] >> 5;
    let level = av1c[1] & 0x1F;
    let tier = if av1c[2] & 0x80 != 0 { 'H' } else { 'M' };
    let depth = if av1c[2] & 0x20 != 0 {
        12
    } else if av1c[2] & 0x40 != 0 {
        10
    } else {
        8
    };
    Some(format!("av01.{profile}.{level:02}{tier}.{depth:02}"))
}

/// Bit depth from an `av1C` record.
pub fn av1_bit_depth(av1c: &[u8]) -> Option<u8> {
    let b = *av1c.get(2)?;
    Some(if b & 0x20 != 0 {
        12
    } else if b & 0x40 != 0 {
        10
    } else {
        8
    })
}

#[cfg(test)]
mod cra_tests {
    use super::*;

    #[test]
    fn cra_becomes_bla_w_lp() {
        // [len 2][CRA hdr 0x2A 0x01] [len 2][PPS 0x44 0x01]
        let mut s = vec![0, 0, 0, 2, 0x2A, 0x01, 0, 0, 0, 2, 0x44, 0x01];
        cra_to_bla(&mut s, 4);
        assert_eq!(s, vec![0, 0, 0, 2, 0x20, 0x01, 0, 0, 0, 2, 0x44, 0x01]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1_string() {
        // marker+version, profile 0 level 8, tier 0 high_bitdepth 1
        assert_eq!(
            av1_codec_string(&[0x81, 0x08, 0x4C, 0x00]).as_deref(),
            Some("av01.0.08M.10")
        );
    }

    #[test]
    fn sei_cll() {
        // HEVC prefix SEI header (type 39), payload 144 size 4: 1000, 400
        let nal = [0x4E, 0x01, 144, 4, 0x03, 0xE8, 0x01, 0x90, 0x80];
        let s = parse_hdr_sei(&nal, 2);
        assert_eq!(
            s.cll,
            Some(ContentLightLevel {
                max_cll: 1000,
                max_fall: 400
            })
        );
    }

    #[test]
    fn strip_ps() {
        // SPS (7), IDR slice (5)
        let s = [0, 0, 0, 2, 0x67, 0x64, 0, 0, 0, 2, 0x65, 0x88];
        assert!(has_inband_parameter_sets(&s, 4, NalFlavor::H264));
        assert_eq!(
            strip_parameter_sets(&s, 4, NalFlavor::H264),
            vec![0, 0, 0, 2, 0x65, 0x88]
        );
    }
}
