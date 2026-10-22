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

/// The coding tools of the Baseline profile that Constrained Baseline, Main
/// and High lack (H.264 A.2.1.1): a stream with `profile_idc` 66 that uses
/// none of them is a Constrained Baseline stream, whether or not its encoder
/// set `constraint_set1_flag`, and every browser decodes it. Decoders of
/// those profiles (FFmpeg's, the hardware ones browsers use) don't implement
/// slice groups or arbitrary slice order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BaselineTools {
    /// Slice groups, "flexible macroblock ordering" (FMO): a PPS with
    /// `num_slice_groups_minus1 > 0`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub slice_groups: bool,
    /// Arbitrary slice order (ASO): a picture whose slices don't start in
    /// increasing macroblock order. Seen only in the pictures sampled.
    #[serde(default, skip_serializing_if = "is_false")]
    pub arbitrary_slice_order: bool,
    /// Redundant pictures: a PPS with `redundant_pic_cnt_present_flag`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub redundant_pictures: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl BaselineTools {
    /// True when any tool is used.
    pub fn any(&self) -> bool {
        self.slice_groups || self.arbitrary_slice_order || self.redundant_pictures
    }

    /// The tools used, by their usual short names ("FMO, ASO").
    pub fn names(&self) -> String {
        let mut v = Vec::new();
        if self.slice_groups {
            v.push("FMO");
        }
        if self.arbitrary_slice_order {
            v.push("ASO");
        }
        if self.redundant_pictures {
            v.push("redundant pictures");
        }
        v.join(", ")
    }

    /// Adds the tools a PPS NAL unit (with its 1-byte header) turns on.
    pub fn add_pps(&mut self, nal: &[u8]) {
        if let Some((fmo, redundant)) = pps_tools(nal) {
            self.slice_groups |= fmo;
            self.redundant_pictures |= redundant;
        }
    }

    /// Adds what one length-prefixed access unit shows: in-band PPSs, and
    /// slices that don't start in increasing macroblock order (ASO).
    /// `sample` may be a prefix of the access unit (only NAL heads are read).
    pub fn add_access_unit(&mut self, sample: &[u8], length_size: usize) {
        let mut last_mb: Option<u32> = None;
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
            match nal.first().map(|h| h & 0x1F) {
                // A whole PPS only (a cut one would read as garbage).
                Some(8) if pos <= sample.len() => self.add_pps(nal),
                Some(1 | 5) => {
                    if let Some(mb) = first_mb_in_slice(nal) {
                        if last_mb.is_some_and(|l| mb <= l) {
                            self.arbitrary_slice_order = true;
                        }
                        last_mb = Some(mb);
                    }
                }
                _ => {}
            }
        }
    }
}

/// (slice groups, redundant pictures) of a PPS NAL unit (its 1-byte header
/// included), ITU-T H.264 §7.3.2.2. `None` when it doesn't parse. With
/// slice groups the rest is not read (the stream needs them either way).
pub fn pps_tools(nal: &[u8]) -> Option<(bool, bool)> {
    if nal.first()? & 0x1F != 8 {
        return None;
    }
    let rbsp = unescape_rbsp(nal.get(1..)?);
    let mut r = BitReader::new(&rbsp);
    r.ue()?; // pic_parameter_set_id
    r.ue()?; // seq_parameter_set_id
    r.skip(1)?; // entropy_coding_mode_flag
    r.skip(1)?; // bottom_field_pic_order_in_frame_present_flag
    if r.ue()? > 0 {
        return Some((true, false));
    }
    r.ue()?; // num_ref_idx_l0_default_active_minus1
    r.ue()?; // num_ref_idx_l1_default_active_minus1
    r.skip(1)?; // weighted_pred_flag
    r.skip(2)?; // weighted_bipred_idc
    r.se()?; // pic_init_qp_minus26
    r.se()?; // pic_init_qs_minus26
    r.se()?; // chroma_qp_index_offset
    r.skip(1)?; // deblocking_filter_control_present_flag
    r.skip(1)?; // constrained_intra_pred_flag
    Some((false, r.bit()?))
}

/// `first_mb_in_slice` of a coded slice NAL unit (types 1 and 5, header
/// byte included): the first field of the slice header (§7.3.3).
pub fn first_mb_in_slice(nal: &[u8]) -> Option<u32> {
    if !matches!(nal.first()? & 0x1F, 1 | 5) {
        return None;
    }
    // The field fits in the first few bytes; unescape only those.
    let head = &nal[1..nal.len().min(17)];
    BitReader::new(&unescape_rbsp(head)).ue()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// MSB-first bit writer for hand-built parameter sets and slices.
    #[derive(Default)]
    pub(crate) struct BitWriter {
        out: Vec<u8>,
        bits: u32,
    }

    impl BitWriter {
        pub(crate) fn put(&mut self, v: u64, n: u32) {
            for i in (0..n).rev() {
                if self.bits.is_multiple_of(8) {
                    self.out.push(0);
                }
                let b = ((v >> i) & 1) as u8;
                *self.out.last_mut().unwrap() |= b << (7 - self.bits % 8);
                self.bits += 1;
            }
        }
        pub(crate) fn ue(&mut self, v: u32) {
            let x = v as u64 + 1;
            let len = 64 - x.leading_zeros();
            self.put(0, len - 1);
            self.put(x, len);
        }
        pub(crate) fn se(&mut self, v: i32) {
            self.ue(if v > 0 { 2 * v as u32 - 1 } else { (-2 * v) as u32 });
        }
        /// RBSP trailing bits, then the bytes.
        pub(crate) fn finish(mut self) -> Vec<u8> {
            self.put(1, 1);
            while !self.bits.is_multiple_of(8) {
                self.put(0, 1);
            }
            self.out
        }
    }

    /// A 64×64 Baseline SPS NAL unit with these constraint flags (0x80:
    /// plain Baseline as phones write it, 0xC0: Constrained Baseline).
    pub(crate) fn baseline_sps(constraints: u8) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(66, 8);
        w.put(u64::from(constraints), 8);
        w.put(30, 8); // level 3.0
        w.ue(0); // sps id
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // pic_order_cnt_type
        w.ue(1); // max_num_ref_frames
        w.put(0, 1);
        w.ue(3); // 4 macroblocks wide
        w.ue(3); // 4 high
        w.put(1, 1); // frame_mbs_only_flag
        w.put(1, 1); // direct_8x8_inference_flag
        w.put(0, 1); // no cropping
        w.put(0, 1); // no VUI
        let mut nal = vec![0x67];
        nal.extend(w.finish());
        nal
    }

    /// A Baseline PPS NAL unit: `slice_groups` > 1 adds a type-0
    /// (interleaved) slice group map with run lengths of 1.
    pub(crate) fn baseline_pps(slice_groups: u32, redundant: bool) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.ue(0); // pps id
        w.ue(0); // sps id
        w.put(0, 1); // CAVLC
        w.put(0, 1);
        w.ue(slice_groups - 1);
        if slice_groups > 1 {
            w.ue(0); // slice_group_map_type 0
            for _ in 0..slice_groups {
                w.ue(0); // run_length_minus1
            }
        }
        w.ue(0);
        w.ue(0);
        w.put(0, 1);
        w.put(0, 2);
        w.se(0);
        w.se(0);
        w.se(0);
        w.put(1, 1); // deblocking_filter_control_present_flag
        w.put(0, 1);
        w.put(redundant as u64, 1);
        let mut nal = vec![0x68];
        nal.extend(w.finish());
        nal
    }

    /// A slice NAL unit (`nal_type` 1 or 5) whose header starts at `first_mb`.
    pub(crate) fn slice(nal_type: u8, first_mb: u32) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.ue(first_mb);
        w.ue(7); // slice_type I (all slices)
        w.ue(0); // pps id
        w.put(0xAAAA_AAAA, 32); // the rest: not parsed
        let mut nal = vec![0x60 | nal_type];
        nal.extend(w.finish());
        nal
    }

    /// A length-prefixed (4-byte) access unit of `nals`.
    pub(crate) fn access_unit(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&(n.len() as u32).to_be_bytes());
            out.extend_from_slice(n);
        }
        out
    }

    #[test]
    fn baseline_tools_from_parameter_sets_and_slices() {
        assert_eq!(pps_tools(&baseline_pps(1, false)), Some((false, false)));
        assert_eq!(pps_tools(&baseline_pps(4, false)), Some((true, false)));
        assert_eq!(pps_tools(&baseline_pps(1, true)), Some((false, true)));
        assert_eq!(pps_tools(&[0x67, 0xEF]), None);
        assert_eq!(pps_tools(&[0x68]), None);
        assert_eq!(first_mb_in_slice(&slice(5, 396)), Some(396));

        // Slices in raster order (0, 99, 198): no ASO.
        let mut t = BaselineTools::default();
        t.add_access_unit(&access_unit(&[slice(5, 0), slice(5, 99), slice(5, 198)]), 4);
        assert!(!t.any());
        // Out of order (99 before 0): ASO.
        t.add_access_unit(&access_unit(&[slice(1, 99), slice(1, 0)]), 4);
        assert!(t.arbitrary_slice_order && !t.slice_groups, "{t:?}");
        assert_eq!(t.names(), "ASO");
        // An in-band PPS with slice groups; a prefix cut inside the last
        // slice still reads the PPS.
        let mut t = BaselineTools::default();
        let au = access_unit(&[baseline_pps(2, false), slice(5, 0), slice(5, 40)]);
        t.add_access_unit(&au[..au.len() - 3], 4);
        assert!(t.slice_groups && !t.arbitrary_slice_order, "{t:?}");
        // Serialized, only the tools used appear.
        assert_eq!(serde_json::to_string(&t).unwrap(), r#"{"slice_groups":true}"#);
        assert_eq!(
            serde_json::from_str::<BaselineTools>("{}").unwrap(),
            BaselineTools::default()
        );
    }

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
