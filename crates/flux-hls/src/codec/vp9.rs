//! VP9: the uncompressed header of a key frame (VP9 bitstream spec §6.2)
//! and the `vpcC` record of VP9's ISO-BMFF binding ("VP Codec ISO Media
//! File Format Binding" v1.0 §2.3), so VP9 from Matroska/WebM or MP4 is
//! packaged into fMP4 as it is (a `vp09` sample entry). A Matroska block
//! and a `vp09` sample hold the same thing (a frame or a superframe), so
//! samples pass through unchanged.

use flux_core::ColorInfo;

use crate::bits::BitReader;

/// What a VP9 key frame's uncompressed header says about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9Header {
    /// 0–3: 8-bit 4:2:0, 8-bit 4:2:2/4:4:0/4:4:4, 10/12-bit 4:2:0, 10/12-bit other.
    pub profile: u8,
    pub bit_depth: u8,
    /// (subsampling_x, subsampling_y): (1, 1) is 4:2:0.
    pub subsampling: (bool, bool),
    pub full_range: bool,
    /// VP9 `color_space`: 0 unknown, 1 BT.601, 2 BT.709, 3 SMPTE 170,
    /// 4 SMPTE 240, 5 BT.2020, 6 reserved, 7 sRGB.
    pub color_space: u8,
    pub width: u32,
    pub height: u32,
}

impl Vp9Header {
    /// The H.273 matrix of the colour space (ffmpeg's VP9 decoder's
    /// mapping; 2 = unspecified).
    pub fn matrix(&self) -> u8 {
        match self.color_space {
            1 => 5,
            2 => 1,
            3 => 6,
            4 => 7,
            5 => 9,
            7 => 0,
            _ => 2,
        }
    }
}

/// Parses the header of the key frame `frame` starts with (a lone frame
/// or the first frame of a superframe), or of the first key frame of a
/// superframe whose index is intact. `None` for other frames and garbage.
pub fn parse_keyframe(frame: &[u8]) -> Option<Vp9Header> {
    if let Some(h) = parse_frame(frame) {
        return Some(h);
    }
    superframe(frame)?.into_iter().find_map(parse_frame)
}

/// The frames of a superframe (Annex B), from the index at its end.
fn superframe(data: &[u8]) -> Option<Vec<&[u8]>> {
    let last = *data.last()?;
    if last & 0xE0 != 0xC0 {
        return None;
    }
    let frames = usize::from(last & 7) + 1;
    let mag = usize::from((last >> 3) & 3) + 1;
    let index_len = 2 + mag * frames;
    let index_start = data.len().checked_sub(index_len)?;
    if data[index_start] != last {
        return None;
    }
    let mut out = Vec::with_capacity(frames);
    let mut pos = 0usize;
    for i in 0..frames {
        let at = index_start + 1 + i * mag;
        let size = (0..mag).fold(0usize, |n, b| n | usize::from(data[at + b]) << (8 * b));
        let end = pos.checked_add(size).filter(|&e| e <= index_start)?;
        out.push(&data[pos..end]);
        pos = end;
    }
    Some(out)
}

fn parse_frame(frame: &[u8]) -> Option<Vp9Header> {
    let mut r = BitReader::new(frame);
    if r.u8(2)? != 2 {
        return None; // frame_marker
    }
    let low = r.u8(1)?;
    let high = r.u8(1)?;
    let profile = (high << 1) | low;
    if profile == 3 {
        r.skip(1)?; // reserved_zero
    }
    if r.bit()? {
        return None; // show_existing_frame: no header follows
    }
    if r.bit()? {
        return None; // frame_type 1: not a key frame
    }
    r.skip(2)?; // show_frame, error_resilient_mode
    if r.u32(24)? != 0x49_83_42 {
        return None; // frame_sync_code
    }
    let bit_depth = if profile >= 2 {
        if r.bit()? {
            12
        } else {
            10
        }
    } else {
        8
    };
    let color_space = r.u8(3)?;
    let (full_range, subsampling) = if color_space != 7 {
        let full = r.bit()?;
        let ss = if profile == 1 || profile == 3 {
            let s = (r.bit()?, r.bit()?);
            r.skip(1)?; // reserved_zero
            s
        } else {
            (true, true)
        };
        (full, ss)
    } else {
        // sRGB: full range 4:4:4 (profiles 1 and 3 only).
        if profile == 1 || profile == 3 {
            r.skip(1)?;
        }
        (true, (false, false))
    };
    let width = r.u32(16)? + 1;
    let height = r.u32(16)? + 1;
    Some(Vp9Header {
        profile,
        bit_depth,
        subsampling,
        full_range,
        color_space,
        width,
        height,
    })
}

/// The features of a Matroska `V_VP9` CodecPrivate (ID, length, value):
/// 1 profile, 2 level, 3 bit depth, 4 chroma subsampling (`vpcC` codes).
fn private_features(b: &[u8]) -> [Option<u8>; 4] {
    let mut out = [None; 4];
    let mut i = 0usize;
    while i + 2 < b.len() {
        let (id, len) = (b[i], usize::from(b[i + 1]));
        if (1..=4).contains(&id) && len == 1 {
            out[usize::from(id) - 1] = Some(b[i + 2]);
        }
        i += 2 + len;
    }
    out
}

/// The VP9 level (10 = 1.0 … 62 = 6.2) of a picture size and frame rate,
/// from the level limits (VP9 levels table; as ffmpeg's `vpcc` writer):
/// the luma picture size, then the luma sample rate when the rate is known.
pub fn level(width: u32, height: u32, fps: Option<f64>) -> u8 {
    let size = u64::from(width) * u64::from(height);
    let rate = fps
        .filter(|f| f.is_finite() && *f > 0.0)
        .map_or(0.0, |f| size as f64 * f);
    const LEVELS: [(u8, f64, u64); 14] = [
        (10, 829_440.0, 36_864),
        (11, 2_764_800.0, 73_728),
        (20, 4_608_000.0, 122_880),
        (21, 9_216_000.0, 245_760),
        (30, 20_736_000.0, 552_960),
        (31, 36_864_000.0, 983_040),
        (40, 83_558_400.0, 2_228_224),
        (41, 160_432_128.0, 2_228_224),
        (50, 311_951_360.0, 8_912_896),
        (51, 588_251_136.0, 8_912_896),
        (52, 1_176_502_272.0, 8_912_896),
        (60, 1_176_502_272.0, 35_651_584),
        (61, 2_353_004_544.0, 35_651_584),
        (62, 4_706_009_088.0, 35_651_584),
    ];
    LEVELS
        .iter()
        .find(|(_, max_rate, max_size)| rate <= *max_rate && size <= *max_size)
        .map_or(62, |l| l.0)
}

/// True for the levels the VP9 spec defines (a codec string with another,
/// such as the 0 many muxers write, fails `isTypeSupported`).
pub fn is_level(l: u8) -> bool {
    matches!(
        l,
        10 | 11 | 20 | 21 | 30 | 31 | 40 | 41 | 50 | 51 | 52 | 60 | 61 | 62
    )
}

/// What a `vp09` sample entry says about a VP9 track: its `vpcC` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9Config {
    pub profile: u8,
    pub level: u8,
    pub bit_depth: u8,
    /// `vpcC` chromaSubsampling: 0 or 1 4:2:0 (left / co-sited), 2 4:2:2,
    /// 3 4:4:4.
    pub chroma_subsampling: u8,
    pub full_range: bool,
    /// H.273 colour code points (2 = unspecified).
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
}

impl Vp9Config {
    /// From the first key frame (`frame`), else the Matroska CodecPrivate
    /// `private`, else profile 0, 8-bit 4:2:0. The level is the
    /// CodecPrivate's, else the picture's ([`level`]); colour code points
    /// are the container's (`color`), else the frame's colour space.
    pub fn new(
        frame: Option<&[u8]>,
        private: &[u8],
        size: (Option<u32>, Option<u32>),
        fps: Option<f64>,
        color: Option<&ColorInfo>,
    ) -> Vp9Config {
        let h = frame.and_then(parse_keyframe);
        let [p_profile, p_level, p_depth, p_chroma] = private_features(private);
        let (w, ht) = match h {
            Some(h) => (h.width, h.height),
            None => (size.0.unwrap_or(0), size.1.unwrap_or(0)),
        };
        let chroma = match h.map(|h| h.subsampling) {
            // ffmpeg's choice for an unknown chroma location.
            Some((true, true)) => 1,
            Some((true, false)) => 2,
            Some((false, _)) => 3,
            None => p_chroma.filter(|&c| c <= 3).unwrap_or(1),
        };
        let frame_matrix = h.map(|h| h.matrix());
        let code = |c: Option<u8>| c.unwrap_or(2);
        Vp9Config {
            profile: h.map(|h| h.profile).or(p_profile).unwrap_or(0),
            level: p_level
                .filter(|&l| is_level(l))
                .unwrap_or_else(|| level(w, ht, fps)),
            bit_depth: h.map(|h| h.bit_depth).or(p_depth).unwrap_or(8),
            chroma_subsampling: chroma,
            full_range: color
                .and_then(|c| c.full_range)
                .or(h.map(|h| h.full_range))
                .unwrap_or(false),
            primaries: code(color.and_then(|c| c.primaries)),
            transfer: code(color.and_then(|c| c.transfer)),
            matrix: color.and_then(|c| c.matrix).or(frame_matrix).unwrap_or(2),
        }
    }

    /// The `vpcC` box body: version 1, flags 0, then the record with no
    /// codec initialization data (VP9 has none).
    pub fn vpcc(&self) -> Vec<u8> {
        vec![
            1,
            0,
            0,
            0,
            self.profile,
            self.level,
            (self.bit_depth << 4) | (self.chroma_subsampling << 1) | self.full_range as u8,
            self.primaries,
            self.transfer,
            self.matrix,
            0,
            0,
        ]
    }

    /// Reads a `vpcC` box body (version 1).
    pub fn from_vpcc(body: &[u8]) -> Option<Vp9Config> {
        if body.len() < 12 || body[0] != 1 {
            return None;
        }
        Some(Vp9Config {
            profile: body[4],
            level: body[5],
            bit_depth: body[6] >> 4,
            chroma_subsampling: (body[6] >> 1) & 7,
            full_range: body[6] & 1 != 0,
            primaries: body[7],
            transfer: body[8],
            matrix: body[9],
        })
    }

    /// The RFC 6381 codec string for HLS `CODECS` (the short form
    /// `vp09.PP.LL.DD`, which `MediaSource.isTypeSupported` takes).
    pub fn codec_string(&self) -> String {
        format!("vp09.{:02}.{:02}.{:02}", self.profile, self.level, self.bit_depth)
    }

    /// The profile as flux-probe (and ffprobe) name it.
    pub fn profile_name(&self) -> String {
        format!("Profile {}", self.profile)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codec::h264::tests::BitWriter;

    /// The first bytes of a VP9 key frame: profile, bit depth (profiles 2
    /// and 3), colour space, range, subsampling (profiles 1 and 3), size.
    /// The rest of the frame (refresh flags, loop filter, …) is not parsed.
    pub(crate) fn keyframe(
        profile: u8,
        ten_or_twelve: bool,
        color_space: u8,
        ss: (bool, bool),
        w: u32,
        h: u32,
    ) -> Vec<u8> {
        let mut b = BitWriter::default();
        b.put(2, 2); // frame_marker
        b.put(u64::from(profile & 1), 1);
        b.put(u64::from(profile >> 1), 1);
        if profile == 3 {
            b.put(0, 1);
        }
        b.put(0, 1); // show_existing_frame
        b.put(0, 1); // frame_type KEY_FRAME
        b.put(1, 1); // show_frame
        b.put(0, 1); // error_resilient_mode
        b.put(0x49_83_42, 24);
        if profile >= 2 {
            b.put(u64::from(ten_or_twelve), 1);
        }
        b.put(u64::from(color_space), 3);
        if color_space != 7 {
            b.put(0, 1); // color_range: studio
            if profile == 1 || profile == 3 {
                b.put(u64::from(ss.0), 1);
                b.put(u64::from(ss.1), 1);
                b.put(0, 1);
            }
        } else if profile == 1 || profile == 3 {
            b.put(0, 1);
        }
        b.put(u64::from(w - 1), 16);
        b.put(u64::from(h - 1), 16);
        b.put(0x5A5A_5A5A, 32); // filler
        b.finish()
    }

    #[test]
    fn key_frame_headers() {
        let h = parse_keyframe(&keyframe(0, false, 2, (true, true), 1920, 1080)).unwrap();
        assert_eq!(
            (
                h.profile,
                h.bit_depth,
                h.subsampling,
                h.color_space,
                h.width,
                h.height
            ),
            (0, 8, (true, true), 2, 1920, 1080)
        );
        let h = parse_keyframe(&keyframe(2, false, 5, (true, true), 3840, 2160)).unwrap();
        assert_eq!((h.profile, h.bit_depth, h.color_space), (2, 10, 5));
        let h = parse_keyframe(&keyframe(3, true, 1, (false, false), 640, 360)).unwrap();
        assert_eq!((h.profile, h.bit_depth, h.subsampling), (3, 12, (false, false)));
        let h = parse_keyframe(&keyframe(1, false, 7, (true, true), 64, 64)).unwrap();
        assert_eq!((h.subsampling, h.full_range), ((false, false), true));
        // An inter frame (frame_type 1) and garbage have no key frame header.
        let mut inter = keyframe(0, false, 2, (true, true), 64, 64);
        inter[0] |= 0x04;
        assert_eq!(parse_keyframe(&inter), None);
        assert_eq!(parse_keyframe(&[0xFF; 16]), None);
        assert_eq!(parse_keyframe(&[]), None);
    }

    #[test]
    fn superframe_index_finds_the_key_frame() {
        // A hidden inter frame first, then the key frame, then the index
        // (2 frames, 1-byte sizes: marker 0b110_00_001).
        let mut hidden = keyframe(0, false, 2, (true, true), 64, 64);
        hidden[0] |= 0x04; // frame_type 1
        let key = keyframe(0, false, 2, (true, true), 320, 240);
        let mut sf = hidden.clone();
        sf.extend_from_slice(&key);
        let marker = 0b1100_0001;
        sf.extend_from_slice(&[marker, hidden.len() as u8, key.len() as u8, marker]);
        let h = parse_keyframe(&sf).unwrap();
        assert_eq!((h.width, h.height), (320, 240));
        // A broken index is ignored.
        let n = sf.len();
        sf[n - 4] = 0;
        assert_eq!(parse_keyframe(&sf), None);
    }

    #[test]
    fn levels_follow_the_vp9_limits() {
        assert_eq!(level(1920, 1080, Some(30.0)), 40);
        assert_eq!(level(1920, 1080, Some(60.0)), 41);
        assert_eq!(level(3840, 2160, Some(30.0)), 50);
        assert_eq!(level(3840, 2160, Some(60.0)), 51);
        assert_eq!(level(1280, 720, Some(30.0)), 31);
        assert_eq!(level(640, 360, None), 21);
        assert_eq!(level(176, 144, Some(15.0)), 10);
        assert_eq!(level(16000, 9000, None), 62);
    }

    #[test]
    fn vpcc_from_the_key_frame_or_codec_private() {
        let key = keyframe(2, false, 5, (true, true), 3840, 2160);
        let pq = ColorInfo {
            primaries: Some(9),
            transfer: Some(16),
            matrix: Some(9),
            full_range: Some(false),
        };
        let c = Vp9Config::new(Some(&key), &[], (None, None), Some(24.0), Some(&pq));
        assert_eq!(c.codec_string(), "vp09.02.50.10");
        assert_eq!(c.profile_name(), "Profile 2");
        assert_eq!(c.vpcc(), vec![1, 0, 0, 0, 2, 50, 0xA2, 9, 16, 9, 0, 0]);
        assert_eq!(Vp9Config::from_vpcc(&c.vpcc()), Some(c));
        // No colour in the container: the frame's colour space gives the matrix.
        let key = keyframe(0, false, 2, (true, true), 1280, 720);
        let c = Vp9Config::new(Some(&key), &[], (None, None), None, None);
        assert_eq!(
            (c.primaries, c.transfer, c.matrix, c.chroma_subsampling),
            (2, 2, 1, 1)
        );
        assert_eq!(c.codec_string(), "vp09.00.31.08");
        // No frame: the CodecPrivate features (profile 0, level 4.1, 8-bit).
        let private = [1, 1, 0, 2, 1, 41, 3, 1, 8, 4, 1, 1];
        let c = Vp9Config::new(None, &private, (Some(1920), Some(1080)), None, None);
        assert_eq!(c.codec_string(), "vp09.00.41.08");
        // Nothing at all: profile 0, 8-bit, level from the size.
        let c = Vp9Config::new(None, &[], (Some(854), Some(480)), Some(30.0), None);
        assert_eq!(c.codec_string(), "vp09.00.30.08");
    }
}
