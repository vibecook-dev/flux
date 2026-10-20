//! H.264/HEVC elementary streams as carried in MPEG-TS: Annex B NAL units
//! (start codes), access-unit inspection for random access, parameter sets
//! → `avcC`/`hvcC`, and Annex B → length-prefixed samples.

use std::collections::HashMap;

use crate::bits::{unescape_rbsp, BitReader};

/// Video codec of a TS stream that can be remuxed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    H264,
    Hevc,
}

impl Flavor {
    pub(crate) fn nal_type(self, nal: &[u8]) -> u8 {
        match self {
            Flavor::H264 => nal.first().map(|b| b & 0x1F).unwrap_or(0),
            Flavor::Hevc => nal.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0),
        }
    }

    fn header_len(self) -> usize {
        match self {
            Flavor::H264 => 1,
            Flavor::Hevc => 2,
        }
    }

    pub(crate) fn is_vcl(self, t: u8) -> bool {
        match self {
            Flavor::H264 => (1..=5).contains(&t),
            Flavor::Hevc => t < 32,
        }
    }

    /// VPS/SPS/PPS (and H.264 SPS extension).
    pub(crate) fn is_parameter_set(self, t: u8) -> bool {
        match self {
            Flavor::H264 => matches!(t, 7 | 8 | 13),
            Flavor::Hevc => matches!(t, 32..=34),
        }
    }

    fn is_sei(self, t: u8) -> bool {
        match self {
            Flavor::H264 => t == 6,
            Flavor::Hevc => t == 39 || t == 40,
        }
    }

    /// Access unit delimiters and filler data: dropped when remuxing.
    fn is_droppable(self, t: u8) -> bool {
        match self {
            Flavor::H264 => matches!(t, 9 | 12),
            Flavor::Hevc => matches!(t, 35 | 38),
        }
    }
}

/// Index of the next `00 00 01` at or after `from`. Eight bytes at a time:
/// a start code needs a zero byte, and most words of coded data have none.
pub(crate) fn next_start_code(b: &[u8], from: usize) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let n = b.len();
    let mut i = from;
    while i + 8 <= n {
        let w = u64::from_le_bytes([
            b[i],
            b[i + 1],
            b[i + 2],
            b[i + 3],
            b[i + 4],
            b[i + 5],
            b[i + 6],
            b[i + 7],
        ]);
        if w.wrapping_sub(LO) & !w & HI != 0 {
            for j in i..i + 8 {
                if j + 2 < n && b[j] == 0 && b[j + 1] == 0 && b[j + 2] == 1 {
                    return Some(j);
                }
            }
        }
        i += 8;
    }
    while i + 2 < n {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Iterates the NAL units of an Annex B buffer: (NAL unit without trailing
/// zero bytes, whether another start code follows it in `b`).
pub(crate) fn nals(b: &[u8]) -> impl Iterator<Item = (&[u8], bool)> {
    let mut next = next_start_code(b, 0);
    std::iter::from_fn(move || {
        let sc = next?;
        let start = sc + 3;
        next = next_start_code(b, start);
        let (end, complete) = match next {
            Some(e) => (e, true),
            None => (b.len(), false),
        };
        let mut e = end;
        while e > start && b[e - 1] == 0 {
            e -= 1;
        }
        Some((&b[start..e], complete))
    })
}

/// Converts one access unit to a length-prefixed sample (4-byte lengths),
/// dropping AUDs and filler data, and parameter sets when `strip_ps`.
pub(crate) fn to_sample(au: &[u8], f: Flavor, strip_ps: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len() + 16);
    for (nal, _) in nals(au) {
        if nal.is_empty() {
            continue;
        }
        let t = f.nal_type(nal);
        if f.is_droppable(t) || (strip_ps && f.is_parameter_set(t)) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Pictures that start in an access unit: VCL NAL units that are the first
/// slice of a picture. One per PES is what the remux expects.
pub(crate) fn picture_starts(au: &[u8], f: Flavor) -> usize {
    nals(au)
        .filter(|(nal, _)| nal.len() > f.header_len() && f.is_vcl(f.nal_type(nal)))
        .filter(|(nal, _)| match f {
            // first_mb_in_slice == 0 ⇔ its ue(v) code is the single bit 1.
            Flavor::H264 => nal[1] & 0x80 != 0,
            Flavor::Hevc => nal[2] & 0x80 != 0,
        })
        .count()
}

// ---------------------------------------------------------------- SPS facts

/// What the remux needs from an H.264 SPS beyond `codec::h264::parse_sps`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AvcSpsFacts {
    pub id: u32,
    pub profile_idc: u8,
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub separate_colour_plane: bool,
    pub log2_max_frame_num: u32,
    pub frame_mbs_only: bool,
}

fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Parses an H.264 SPS NAL unit (with its header) up to `frame_mbs_only_flag`.
pub(crate) fn avc_sps_facts(nal: &[u8]) -> Option<AvcSpsFacts> {
    if nal.len() < 4 || nal[0] & 0x1F != 7 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.u8(8)?;
    r.skip(16)?; // constraint flags, level_idc
    let id = r.ue()?;
    let mut f = AvcSpsFacts {
        id,
        profile_idc,
        chroma_format_idc: 1,
        bit_depth_luma: 8,
        bit_depth_chroma: 8,
        ..Default::default()
    };
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        f.chroma_format_idc = r.ue()?;
        if f.chroma_format_idc == 3 {
            f.separate_colour_plane = r.bit()?;
        }
        f.bit_depth_luma = 8 + r.ue()?.min(8) as u8;
        f.bit_depth_chroma = 8 + r.ue()?.min(8) as u8;
        r.skip(1)?;
        if r.bit()? {
            let n = if f.chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..n {
                if r.bit()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    f.log2_max_frame_num = r.ue()?.min(12) + 4;
    let poc_type = r.ue()?;
    if poc_type == 0 {
        r.ue()?;
    } else if poc_type == 1 {
        r.skip(1)?;
        r.se()?;
        r.se()?;
        for _ in 0..r.ue()?.min(256) {
            r.se()?;
        }
    }
    r.ue()?; // max_num_ref_frames
    r.skip(1)?;
    r.ue()?;
    r.ue()?;
    f.frame_mbs_only = r.bit()?;
    Some(f)
}

/// What the remux needs from an HEVC SPS beyond `codec::hevc::parse_sps`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HevcSpsFacts {
    pub id: u32,
    /// general_profile_space … general_level_idc: the 12 bytes `hvcC` copies.
    pub ptl: [u8; 12],
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting: bool,
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
}

/// Parses an HEVC SPS NAL unit (with its 2-byte header) up to the bit depths.
pub(crate) fn hevc_sps_facts(nal: &[u8]) -> Option<HevcSpsFacts> {
    if nal.len() < 16 || (nal[0] >> 1) & 0x3F != 33 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[2..]);
    let mut ptl = [0u8; 12];
    ptl.copy_from_slice(rbsp.get(1..13)?);
    let mut r = BitReader::new(&rbsp);
    r.skip(4)?;
    let max_sub_layers_minus1 = r.u8(3)?;
    let temporal_id_nesting = r.bit()?;
    r.skip(96)?; // general profile/tier/level
    let n = max_sub_layers_minus1 as usize;
    let mut flags = [(false, false); 8];
    for f in flags.iter_mut().take(n) {
        *f = (r.bit()?, r.bit()?);
    }
    if n > 0 {
        r.skip(2 * (8 - n))?;
    }
    for &(profile, level) in flags.iter().take(n) {
        if profile {
            r.skip(88)?;
        }
        if level {
            r.skip(8)?;
        }
    }
    let id = r.ue()?;
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc == 3 {
        r.skip(1)?;
    }
    r.ue()?;
    r.ue()?;
    if r.bit()? {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let bit_depth_luma = 8 + r.ue()?.min(8) as u8;
    let bit_depth_chroma = 8 + r.ue()?.min(8) as u8;
    Some(HevcSpsFacts {
        id,
        ptl,
        max_sub_layers_minus1,
        temporal_id_nesting,
        chroma_format_idc,
        bit_depth_luma,
        bit_depth_chroma,
    })
}

/// First ue(v) of a NAL unit's RBSP (the PPS id), and the second (H.264:
/// the SPS a PPS refers to).
fn first_two_ue(nal: &[u8], f: Flavor) -> Option<(u32, u32)> {
    let rbsp = unescape_rbsp(
        nal.get(f.header_len()..)?
            .get(..nal.len().min(32) - f.header_len())?,
    );
    let mut r = BitReader::new(&rbsp);
    Some((r.ue()?, r.ue()?))
}

/// The (type, id) a parameter set is known by, to tell a repeat from a change.
pub(crate) fn parameter_set_key(nal: &[u8], f: Flavor) -> Option<(u8, u32)> {
    let t = f.nal_type(nal);
    let id = match (f, t) {
        (Flavor::H264, 7) => avc_sps_facts(nal)?.id,
        (Flavor::H264, 8) => first_two_ue(nal, f)?.0,
        (Flavor::H264, 13) => first_two_ue(nal, f)?.0,
        (Flavor::Hevc, 32) => (*nal.get(2)? >> 4) as u32,
        (Flavor::Hevc, 33) => hevc_sps_facts(nal).map(|s| s.id).unwrap_or(0),
        (Flavor::Hevc, 34) => first_two_ue(nal, f)?.0,
        _ => return None,
    };
    Some((t, id))
}

// ---------------------------------------------------------------- records

/// `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §5.3.3.1) from SPS and
/// PPS NAL units, with 4-byte NAL lengths.
pub(crate) fn avcc(sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Option<Vec<u8>> {
    let first = sps.first()?;
    if first.len() < 4 || pps.is_empty() {
        return None;
    }
    let mut v = vec![
        1,
        first[1],
        first[2],
        first[3],
        0xFF,
        0xE0 | sps.len().min(31) as u8,
    ];
    for s in sps.iter().take(31) {
        v.extend_from_slice(&(s.len() as u16).to_be_bytes());
        v.extend_from_slice(s);
    }
    v.push(pps.len().min(255) as u8);
    for p in pps.iter().take(255) {
        v.extend_from_slice(&(p.len() as u16).to_be_bytes());
        v.extend_from_slice(p);
    }
    if let Some(f) = avc_sps_facts(first) {
        if matches!(f.profile_idc, 100 | 110 | 122 | 144) {
            v.push(0xFC | f.chroma_format_idc as u8 & 3);
            v.push(0xF8 | (f.bit_depth_luma - 8) & 7);
            v.push(0xF8 | (f.bit_depth_chroma - 8) & 7);
            v.push(0); // no SPS extensions
        }
    }
    Some(v)
}

/// `HEVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §8.3.3.1) from VPS,
/// SPS and PPS NAL units, with 4-byte NAL lengths.
pub(crate) fn hvcc(vps: &[Vec<u8>], sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Option<Vec<u8>> {
    let f = hevc_sps_facts(sps.first()?)?;
    if vps.is_empty() || pps.is_empty() {
        return None;
    }
    let mut v = vec![1];
    v.extend_from_slice(&f.ptl);
    v.extend_from_slice(&[0xF0, 0x00, 0xFC]); // min_spatial_segmentation 0, parallelism 0
    v.push(0xFC | f.chroma_format_idc as u8 & 3);
    v.push(0xF8 | (f.bit_depth_luma - 8) & 7);
    v.push(0xF8 | (f.bit_depth_chroma - 8) & 7);
    v.extend_from_slice(&[0, 0]); // avgFrameRate
    v.push(((f.max_sub_layers_minus1 + 1) << 3) | ((f.temporal_id_nesting as u8) << 2) | 3);
    v.push(3);
    for (t, list) in [(32u8, vps), (33, sps), (34, pps)] {
        v.push(0x80 | t);
        v.extend_from_slice(&(list.len() as u16).to_be_bytes());
        for n in list {
            v.extend_from_slice(&(n.len() as u16).to_be_bytes());
            v.extend_from_slice(n);
        }
    }
    Some(v)
}

// ---------------------------------------------------------------- access units

/// What the start of an access unit says about random access.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AuInfo {
    /// NAL type of the first slice.
    pub vcl_type: u8,
    /// H.264 IDR / HEVC IDR (IDR_W_RADL, IDR_N_LP).
    pub idr: bool,
    /// HEVC IRAP (IDR, CRA, BLA).
    pub irap: bool,
    /// H.264: the first slice is an I or SI slice.
    pub intra: bool,
    /// H.264: a recovery point SEI precedes the slice.
    pub recovery_point: bool,
    /// H.264: the picture is a field (`field_pic_flag`), when known.
    pub field: Option<bool>,
    /// Complete parameter-set NAL units before the first slice.
    pub params: Vec<Vec<u8>>,
    /// Complete SEI NAL units before the first slice.
    pub sei: Vec<Vec<u8>>,
}

/// SPS/PPS seen so far in a stream (H.264 slice headers need them).
#[derive(Debug, Default)]
pub(crate) struct ParamCtx {
    sps: HashMap<u32, AvcSpsFacts>,
    pps_sps: HashMap<u32, u32>,
}

impl ParamCtx {
    pub(crate) fn learn(&mut self, nal: &[u8], f: Flavor) {
        if f != Flavor::H264 {
            return;
        }
        match f.nal_type(nal) {
            7 => {
                if let Some(s) = avc_sps_facts(nal) {
                    self.sps.insert(s.id, s);
                }
            }
            8 => {
                if let Some((pps, sps)) = first_two_ue(nal, f) {
                    self.pps_sps.insert(pps, sps);
                }
            }
            _ => {}
        }
    }
}

/// H.264 SEI payload types of one SEI NAL unit (§7.3.2.3).
fn sei_payload_types(nal: &[u8]) -> Vec<u32> {
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut out = Vec::new();
    let mut p = 0usize;
    while p < rbsp.len() && rbsp[p] != 0x80 {
        let mut ty = 0u32;
        while rbsp.get(p) == Some(&0xFF) {
            ty += 255;
            p += 1;
        }
        let Some(&b) = rbsp.get(p) else { break };
        ty += b as u32;
        p += 1;
        let mut size = 0usize;
        while rbsp.get(p) == Some(&0xFF) {
            size += 255;
            p += 1;
        }
        let Some(&b) = rbsp.get(p) else { break };
        size += b as usize;
        p += 1 + size;
        out.push(ty);
    }
    out
}

/// H.264 slice header: (slice_type, field_pic_flag when the SPS is known).
fn avc_slice(nal: &[u8], ctx: &ParamCtx) -> Option<(u32, Option<bool>)> {
    let rbsp = unescape_rbsp(&nal[1..nal.len().min(40)]);
    let mut r = BitReader::new(&rbsp);
    r.ue()?; // first_mb_in_slice
    let slice_type = r.ue()?;
    let pps = r.ue()?;
    let field = (|| {
        let sps = ctx.sps.get(ctx.pps_sps.get(&pps)?)?;
        if sps.frame_mbs_only {
            return Some(false);
        }
        if sps.separate_colour_plane {
            r.skip(2)?;
        }
        r.skip(sps.log2_max_frame_num as usize)?;
        r.bit()
    })();
    Some((slice_type, field))
}

/// Inspects the start of an access unit up to its first slice. `None` until
/// `head` holds the slice header (call again with more bytes), or when the
/// access unit has no slice in its first 64 KB.
pub(crate) fn inspect(head: &[u8], f: Flavor, ctx: &mut ParamCtx) -> Option<AuInfo> {
    let mut info = AuInfo::default();
    for (nal, complete) in nals(head) {
        if nal.len() < f.header_len() {
            continue;
        }
        let t = f.nal_type(nal);
        if f.is_vcl(t) {
            if nal.len() < 12 && !complete {
                return None;
            }
            info.vcl_type = t;
            match f {
                Flavor::H264 => {
                    info.idr = t == 5;
                    let (slice_type, field) = avc_slice(nal, ctx)?;
                    info.intra = matches!(slice_type % 5, 2 | 4);
                    info.field = field;
                }
                Flavor::Hevc => {
                    info.irap = (16..=23).contains(&t);
                    info.idr = t == 19 || t == 20;
                }
            }
            return Some(info);
        }
        if !complete {
            return None;
        }
        if f.is_parameter_set(t) {
            ctx.learn(nal, f);
            info.params.push(nal.to_vec());
        } else if f.is_sei(t) {
            if f == Flavor::H264 && sei_payload_types(nal).contains(&6) {
                info.recovery_point = true;
            }
            info.sei.push(nal.to_vec());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Parameter sets of media/seeds/seed_h264_ts.ts (x264 High@3.1, 1280x720)
    // and media/probe-fixtures/hevc_mp2_eac3.ts (x265 Main@3.1, 1280x720).
    const AVC_SPS: &str = "6764001facd9405005bb011000000300100000030300f1831960";
    const AVC_PPS: &str = "68efbcb0";
    const HEVC_VPS: &str = "40010c01ffff01600000030090000003000003005d959409";
    const HEVC_SPS: &str =
        "42010101600000030090000003000003005da00280802d16595952930bc05a020000030002000003003010";
    const HEVC_PPS: &str = "4401c073c189";

    #[test]
    fn start_codes_and_nals() {
        let mut b = vec![0xAA; 13];
        b.extend([
            0, 0, 0, 1, 0x09, 0x10, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x65, 7, 7, 0, 0,
        ]);
        assert_eq!(next_start_code(&b, 0), Some(14));
        let n: Vec<(Vec<u8>, bool)> = nals(&b).map(|(n, c)| (n.to_vec(), c)).collect();
        assert_eq!(
            n,
            vec![
                (vec![0x09, 0x10], true),
                (vec![0x67, 1, 2], true),
                (vec![0x65, 7, 7], false)
            ]
        );
        assert_eq!(to_sample(&b, Flavor::H264, true), vec![0, 0, 0, 3, 0x65, 7, 7]);
        assert_eq!(
            to_sample(&b, Flavor::H264, false),
            vec![0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 3, 0x65, 7, 7]
        );
        // Long runs without zero bytes; a start code straddling 8-byte words.
        let mut long = vec![0x11u8; 70];
        long.extend([0, 0, 1, 0x41]);
        assert_eq!(next_start_code(&long, 0), Some(70));
        assert_eq!(next_start_code(&long, 71), None);
    }

    #[test]
    fn avc_sps_and_record() {
        let (sps, pps) = (hex(AVC_SPS), hex(AVC_PPS));
        let f = avc_sps_facts(&sps).unwrap();
        assert_eq!(
            (f.profile_idc, f.chroma_format_idc, f.bit_depth_luma),
            (100, 1, 8)
        );
        assert!(f.frame_mbs_only);
        let rec = avcc(std::slice::from_ref(&sps), std::slice::from_ref(&pps)).unwrap();
        let c = crate::codec::h264::AvcConfig::parse(&rec).unwrap();
        assert_eq!(c.codec_string(), "avc1.64001f");
        assert_eq!(
            (c.length_size, c.sps, c.pps),
            (4, vec![sps.clone()], vec![pps.clone()])
        );
        // High profile: chroma format and bit depths follow the PPS list.
        assert_eq!(&rec[rec.len() - 4..], &[0xFD, 0xF8, 0xF8, 0]);
        let s = crate::codec::h264::parse_sps(&sps).unwrap();
        assert_eq!((s.width, s.height), (1280, 720));
        assert_eq!(parameter_set_key(&sps, Flavor::H264), Some((7, 0)));
        assert_eq!(parameter_set_key(&pps, Flavor::H264), Some((8, 0)));
    }

    #[test]
    fn inspect_finds_idr_and_waits_for_more_bytes() {
        let mut au = vec![0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1];
        au.extend(hex(AVC_SPS));
        au.extend([0, 0, 0, 1]);
        au.extend(hex(AVC_PPS));
        au.extend([0, 0, 1]);
        au.extend(hex("65888403ffd4d44633d7c84b9a3e"));
        let mut ctx = ParamCtx::default();
        assert_eq!(inspect(&au[..au.len() - 14], Flavor::H264, &mut ctx), None);
        let i = inspect(&au, Flavor::H264, &mut ctx).unwrap();
        assert!(i.idr && i.intra);
        assert_eq!(i.field, Some(false));
        assert_eq!(i.params.len(), 2);
        assert_eq!(picture_starts(&au, Flavor::H264), 1);
        // A P slice (from the same stream) is neither IDR nor intra.
        let mut p = vec![0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1];
        p.extend(hex("419a2218ffc61784a556e16f"));
        let i = inspect(&p, Flavor::H264, &mut ctx).unwrap();
        assert!(!i.idr && !i.intra);
    }

    #[test]
    fn hevc_record_from_parameter_sets() {
        let (vps, sps, pps) = (hex(HEVC_VPS), hex(HEVC_SPS), hex(HEVC_PPS));
        let f = hevc_sps_facts(&sps).unwrap();
        assert_eq!(
            (f.chroma_format_idc, f.bit_depth_luma, f.max_sub_layers_minus1),
            (1, 8, 0)
        );
        let rec = hvcc(&[vps], std::slice::from_ref(&sps), &[pps]).unwrap();
        let c = crate::codec::hevc::HevcConfig::parse(&rec).unwrap();
        assert_eq!(c.codec_string("hvc1"), "hvc1.1.6.L93.90");
        assert_eq!((c.length_size, c.arrays.len()), (4, 3));
        let s = crate::codec::hevc::parse_sps(&sps).unwrap();
        assert_eq!((s.width, s.height), (1280, 720));
    }
}
