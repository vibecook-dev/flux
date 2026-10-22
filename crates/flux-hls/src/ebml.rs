//! Minimal EBML (Matroska) primitives: variable-length integers, element
//! headers and the element IDs the playback index needs.
//!
//! Spec: RFC 8794 (EBML) and RFC 9559 (Matroska).

/// Reads an EBML element ID (1–4 bytes, marker bit kept). Advances `p`.
pub(crate) fn read_id(b: &[u8], p: &mut usize) -> Option<u32> {
    let first = *b.get(*p)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 || *p + len > b.len() {
        return None;
    }
    let mut v: u32 = 0;
    for i in 0..len {
        v = (v << 8) | b[*p + i] as u32;
    }
    *p += len;
    Some(v)
}

/// Reads an EBML data size (1–8 bytes, marker bit stripped). Advances `p`.
/// Returns `Some(None)` for the reserved "unknown size" value (all ones).
pub(crate) fn read_size(b: &[u8], p: &mut usize) -> Option<Option<u64>> {
    let first = *b.get(*p)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || *p + len > b.len() {
        return None;
    }
    let mask = if len == 8 { 0 } else { (1u64 << (8 - len)) - 1 };
    let mut v: u64 = (first as u64) & mask;
    let mut all_ones = v == mask;
    for i in 1..len {
        let byte = b[*p + i];
        all_ones &= byte == 0xFF;
        v = (v << 8) | byte as u64;
    }
    *p += len;
    Some(if all_ones { None } else { Some(v) })
}

/// Reads a signed EBML lacing delta (EBML lacing): the vint minus its bias.
pub(crate) fn read_signed_vint(b: &[u8], p: &mut usize) -> Option<i64> {
    let start = *p;
    let raw = read_size(b, p)?.unwrap_or(0) as i64;
    let len = (*p - start) as u32;
    let bias = (1i64 << (7 * len - 1)) - 1;
    Some(raw - bias)
}

/// Big-endian unsigned integer element body.
pub(crate) fn uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0u64, |a, &x| (a << 8) | x as u64)
}

/// IEEE float element body (4 or 8 bytes).
pub(crate) fn float(b: &[u8]) -> f64 {
    match b.len() {
        4 => f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
        8 => f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        _ => 0.0,
    }
}

/// UTF-8 string element body, trimmed of trailing NULs.
pub(crate) fn string(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_end_matches('\0').to_string()
}

/// One child element of an in-memory master element.
pub(crate) struct Child<'a> {
    pub id: u32,
    pub body: &'a [u8],
}

/// Iterates the children of an in-memory master element body. Stops at the
/// first malformed or unknown-size child (unknown sizes only occur for
/// Segment/Cluster, which are never parsed in memory as a whole).
pub(crate) fn children(b: &[u8]) -> impl Iterator<Item = Child<'_>> {
    let mut p = 0usize;
    std::iter::from_fn(move || {
        if p >= b.len() {
            return None;
        }
        let id = read_id(b, &mut p)?;
        let size = read_size(b, &mut p)??;
        let end = p.checked_add(usize::try_from(size).ok()?)?.min(b.len());
        let body = &b[p..end];
        p = end;
        Some(Child { id, body })
    })
}

// Element IDs (RFC 9559 §5.1).
pub(crate) const EBML: u32 = 0x1A45_DFA3;
pub(crate) const DOC_TYPE: u32 = 0x4282;
pub(crate) const SEGMENT: u32 = 0x1853_8067;
pub(crate) const SEEK_HEAD: u32 = 0x114D_9B74;
pub(crate) const SEEK: u32 = 0x4DBB;
pub(crate) const SEEK_ID: u32 = 0x53AB;
pub(crate) const SEEK_POSITION: u32 = 0x53AC;
pub(crate) const INFO: u32 = 0x1549_A966;
pub(crate) const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
pub(crate) const DURATION: u32 = 0x4489;
pub(crate) const TRACKS: u32 = 0x1654_AE6B;
pub(crate) const TRACK_ENTRY: u32 = 0xAE;
pub(crate) const TRACK_NUMBER: u32 = 0xD7;
pub(crate) const TRACK_TYPE: u32 = 0x83;
pub(crate) const FLAG_DEFAULT: u32 = 0x88;
pub(crate) const FLAG_FORCED: u32 = 0x55AA;
pub(crate) const FLAG_HEARING_IMPAIRED: u32 = 0x55AB;
pub(crate) const FLAG_ENABLED: u32 = 0xB9;
pub(crate) const DEFAULT_DURATION: u32 = 0x23_E383;
pub(crate) const NAME: u32 = 0x536E;
pub(crate) const LANGUAGE: u32 = 0x22_B59C;
pub(crate) const LANGUAGE_BCP47: u32 = 0x22_B59D;
pub(crate) const CODEC_ID: u32 = 0x86;
pub(crate) const CODEC_PRIVATE: u32 = 0x63A2;
pub(crate) const CODEC_DELAY: u32 = 0x56AA;
pub(crate) const BLOCK_ADDITION_MAPPING: u32 = 0x41E4;
pub(crate) const BLOCK_ADD_ID_TYPE: u32 = 0x41E7;
pub(crate) const BLOCK_ADD_ID_EXTRA_DATA: u32 = 0x41ED;
pub(crate) const VIDEO: u32 = 0xE0;
pub(crate) const PIXEL_WIDTH: u32 = 0xB0;
pub(crate) const PIXEL_HEIGHT: u32 = 0xBA;
pub(crate) const DISPLAY_WIDTH: u32 = 0x54B0;
pub(crate) const DISPLAY_HEIGHT: u32 = 0x54BA;
pub(crate) const DISPLAY_UNIT: u32 = 0x54B2;
pub(crate) const FLAG_INTERLACED: u32 = 0x9A;
pub(crate) const COLOUR: u32 = 0x55B0;
pub(crate) const MATRIX_COEFFICIENTS: u32 = 0x55B1;
pub(crate) const RANGE: u32 = 0x55B9;
pub(crate) const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
pub(crate) const PRIMARIES: u32 = 0x55BB;
pub(crate) const MAX_CLL: u32 = 0x55BC;
pub(crate) const MAX_FALL: u32 = 0x55BD;
pub(crate) const MASTERING_METADATA: u32 = 0x55D0;
pub(crate) const PRIMARY_R_X: u32 = 0x55D1;
pub(crate) const PRIMARY_R_Y: u32 = 0x55D2;
pub(crate) const PRIMARY_G_X: u32 = 0x55D3;
pub(crate) const PRIMARY_G_Y: u32 = 0x55D4;
pub(crate) const PRIMARY_B_X: u32 = 0x55D5;
pub(crate) const PRIMARY_B_Y: u32 = 0x55D6;
pub(crate) const WHITE_POINT_X: u32 = 0x55D7;
pub(crate) const WHITE_POINT_Y: u32 = 0x55D8;
pub(crate) const LUMINANCE_MAX: u32 = 0x55D9;
pub(crate) const LUMINANCE_MIN: u32 = 0x55DA;
pub(crate) const AUDIO: u32 = 0xE1;
pub(crate) const SAMPLING_FREQUENCY: u32 = 0xB5;
pub(crate) const CHANNELS: u32 = 0x9F;
pub(crate) const BIT_DEPTH: u32 = 0x6264;
pub(crate) const CONTENT_ENCODINGS: u32 = 0x6D80;
pub(crate) const CONTENT_ENCODING: u32 = 0x6240;
pub(crate) const CONTENT_ENCODING_TYPE: u32 = 0x5033;
pub(crate) const CONTENT_COMPRESSION: u32 = 0x5034;
pub(crate) const CONTENT_COMP_ALGO: u32 = 0x4254;
pub(crate) const CONTENT_COMP_SETTINGS: u32 = 0x4255;
pub(crate) const CUES: u32 = 0x1C53_BB6B;
pub(crate) const CUE_POINT: u32 = 0xBB;
pub(crate) const CUE_TIME: u32 = 0xB3;
pub(crate) const CUE_TRACK_POSITIONS: u32 = 0xB7;
pub(crate) const CUE_TRACK: u32 = 0xF7;
pub(crate) const CUE_CLUSTER_POSITION: u32 = 0xF1;
pub(crate) const CUE_RELATIVE_POSITION: u32 = 0xF0;
pub(crate) const CUE_DURATION: u32 = 0xB2;
pub(crate) const CLUSTER: u32 = 0x1F43_B675;
pub(crate) const CLUSTER_TIMESTAMP: u32 = 0xE7;
pub(crate) const CRC32: u32 = 0xBF;
pub(crate) const VOID: u32 = 0xEC;
pub(crate) const SIMPLE_BLOCK: u32 = 0xA3;
pub(crate) const BLOCK_GROUP: u32 = 0xA0;
pub(crate) const BLOCK: u32 = 0xA1;
pub(crate) const BLOCK_DURATION: u32 = 0x9B;
pub(crate) const REFERENCE_BLOCK: u32 = 0xFB;
pub(crate) const CHAPTERS: u32 = 0x1043_A770;
pub(crate) const TAGS: u32 = 0x1254_C367;
pub(crate) const ATTACHMENTS: u32 = 0x1941_A469;

/// True for IDs that can only appear at Segment level. Used to detect the end
/// of an unknown-size Cluster.
pub(crate) fn is_top_level(id: u32) -> bool {
    matches!(
        id,
        CLUSTER | CUES | SEEK_HEAD | INFO | TRACKS | CHAPTERS | TAGS | ATTACHMENTS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_sizes() {
        let mut p = 0;
        assert_eq!(read_size(&[0x81], &mut p), Some(Some(1)));
        p = 0;
        assert_eq!(read_size(&[0x40, 0x02], &mut p), Some(Some(2)));
        p = 0;
        assert_eq!(read_size(&[0xFF], &mut p), Some(None));
        p = 0;
        assert_eq!(
            read_size(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], &mut p),
            Some(None)
        );
        p = 0;
        assert_eq!(read_size(&[0x01, 0, 0, 0, 0, 0, 1, 0], &mut p), Some(Some(256)));
    }

    #[test]
    fn ids() {
        let mut p = 0;
        assert_eq!(read_id(&[0x1A, 0x45, 0xDF, 0xA3], &mut p), Some(EBML));
        p = 0;
        assert_eq!(read_id(&[0xA3], &mut p), Some(SIMPLE_BLOCK));
    }

    #[test]
    fn signed_lace_delta() {
        // 1-byte vint: bias 63. 0xBF = 63 -> 0; 0x80 = 0 -> -63
        let mut p = 0;
        assert_eq!(read_signed_vint(&[0xBF], &mut p), Some(0));
        p = 0;
        assert_eq!(read_signed_vint(&[0x80], &mut p), Some(-63));
        p = 0;
        assert_eq!(read_signed_vint(&[0xC0], &mut p), Some(1));
    }

    #[test]
    fn children_iter() {
        // TrackNumber(1) + CodecID("A")
        let b = [0xD7, 0x81, 0x01, 0x86, 0x81, b'A'];
        let c: Vec<_> = children(&b).map(|c| (c.id, c.body.to_vec())).collect();
        assert_eq!(c, vec![(TRACK_NUMBER, vec![1]), (CODEC_ID, b"A".to_vec())]);
    }
}
