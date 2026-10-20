//! Feeding codecs that have no self-delimiting elementary stream to ffmpeg.
//!
//! The audio transcoder pipes source frames to ffmpeg. TrueHD, DTS, AC-3,
//! FLAC, MP3 and ADTS AAC can be concatenated as they are; Opus and Vorbis
//! packets cannot (they need their headers and packet boundaries), so they go
//! through a minimal Matroska stream: EBML header, one audio track and one
//! SimpleBlock per packet. PCM goes raw (`-f s16le -ar … -ac …`).
//!
//! To place decoded samples exactly, the transcoder also needs each packet's
//! decoded length: Opus from the TOC byte ([`super::audio::opus_packet_samples`]),
//! Vorbis from the mode's block size ([`VorbisBlocks`]).

/// Appends an EBML variable-size integer (8-byte form for unknown sizes).
fn vint_size(out: &mut Vec<u8>, size: Option<u64>) {
    match size {
        None => out.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
        Some(n) => {
            // Smallest length whose all-ones value is reserved: n < 2^(7k) − 1.
            let mut len = 1;
            while len < 8 && n >= (1u64 << (7 * len)) - 1 {
                len += 1;
            }
            let marked = n | (1u64 << (7 * len));
            out.extend_from_slice(&marked.to_be_bytes()[8 - len..]);
        }
    }
}

fn id(out: &mut Vec<u8>, id: u32) {
    let b = id.to_be_bytes();
    let skip = b.iter().take_while(|&&x| x == 0).count().min(3);
    out.extend_from_slice(&b[skip..]);
}

fn element(out: &mut Vec<u8>, eid: u32, body: &[u8]) {
    id(out, eid);
    vint_size(out, Some(body.len() as u64));
    out.extend_from_slice(body);
}

fn uint(out: &mut Vec<u8>, eid: u32, v: u64) {
    let b = v.to_be_bytes();
    let skip = b.iter().take_while(|&&x| x == 0).count().min(7);
    element(out, eid, &b[skip..]);
}

/// An audio track for [`matroska_stream`].
pub struct MkvAudioTrack<'a> {
    /// Matroska CodecID, e.g. `A_OPUS`, `A_VORBIS`.
    pub codec_id: &'a str,
    pub codec_private: &'a [u8],
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_depth: Option<u8>,
}

/// A Matroska stream holding `frames` (timestamp in ms, payload) of one
/// audio track, for `ffmpeg -f matroska -i pipe:0`. Timestamps only need to
/// increase: the transcoder renumbers samples (`asetpts=N/SR/TB`) before
/// cutting, so placement comes from packet durations, not timestamps.
pub fn matroska_stream(track: &MkvAudioTrack<'_>, frames: &[(i64, &[u8])]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames.iter().map(|f| f.1.len() + 8).sum::<usize>() + 512);
    let mut ebml = Vec::new();
    uint(&mut ebml, 0x4286, 1); // EBMLVersion
    uint(&mut ebml, 0x42F7, 1); // EBMLReadVersion
    uint(&mut ebml, 0x42F2, 4); // EBMLMaxIDLength
    uint(&mut ebml, 0x42F3, 8); // EBMLMaxSizeLength
    element(&mut ebml, 0x4282, b"matroska"); // DocType
    uint(&mut ebml, 0x4287, 4); // DocTypeVersion
    uint(&mut ebml, 0x4285, 2); // DocTypeReadVersion
    element(&mut out, 0x1A45DFA3, &ebml);

    id(&mut out, 0x18538067); // Segment, unknown size
    vint_size(&mut out, None);
    let mut info = Vec::new();
    uint(&mut info, 0x2AD7B1, 1_000_000); // TimestampScale: 1 ms
    element(&mut out, 0x1549A966, &info);

    let mut audio = Vec::new();
    element(&mut audio, 0xB5, &(track.sample_rate as f64).to_be_bytes()); // SamplingFrequency
    uint(&mut audio, 0x9F, track.channels.max(1) as u64); // Channels
    if let Some(b) = track.bit_depth {
        uint(&mut audio, 0x6264, b as u64); // BitDepth
    }
    let mut entry = Vec::new();
    uint(&mut entry, 0xD7, 1); // TrackNumber
    uint(&mut entry, 0x73C5, 1); // TrackUID
    uint(&mut entry, 0x83, 2); // TrackType: audio
    element(&mut entry, 0x86, track.codec_id.as_bytes()); // CodecID
    if !track.codec_private.is_empty() {
        element(&mut entry, 0x63A2, track.codec_private);
    }
    element(&mut entry, 0xE1, &audio);
    let mut tracks = Vec::new();
    element(&mut tracks, 0xAE, &entry);
    element(&mut out, 0x1654AE6B, &tracks);

    // Clusters of ≤ 30 s (SimpleBlock timestamps are 16-bit relative).
    let mut i = 0;
    while i < frames.len() {
        let base = frames[i].0;
        let mut cluster = Vec::new();
        uint(&mut cluster, 0xE7, base.max(0) as u64); // Timestamp
        while i < frames.len() && frames[i].0 - base <= 30_000 {
            let (ts, data) = frames[i];
            let rel = (ts - base).clamp(0, i16::MAX as i64) as i16;
            let mut block = Vec::with_capacity(data.len() + 4);
            block.push(0x81); // track 1
            block.extend_from_slice(&rel.to_be_bytes());
            block.push(0x80); // keyframe
            block.extend_from_slice(data);
            element(&mut cluster, 0xA3, &block);
            i += 1;
        }
        element(&mut out, 0x1F43B675, &cluster);
    }
    out
}

/// Opus with its pre-skip zeroed: the decoder then outputs every sample of
/// the first packet fed, so the transcoder can place it (it trims pre-roll
/// itself), instead of silently dropping `pre_skip` samples mid-stream.
pub fn opus_head_without_preskip(head: &[u8]) -> Vec<u8> {
    let mut h = head.to_vec();
    if h.len() >= 12 && &h[..8] == b"OpusHead" {
        h[10] = 0;
        h[11] = 0;
    }
    h
}

/// Decoded length of Vorbis audio packets (Vorbis I §4.3.1 and §1.3.2):
/// a packet returns `prev_blocksize/4 + cur_blocksize/4` samples, and the
/// first packet decoded returns none.
#[derive(Debug, Clone)]
pub struct VorbisBlocks {
    blocksize: [u32; 2],
    /// Block flag per mode (true = long block).
    mode_long: Vec<bool>,
    mode_bits: u32,
    prev: Option<u32>,
}

impl VorbisBlocks {
    /// Parses the identification and setup headers from Matroska
    /// `A_VORBIS` CodecPrivate (Xiph-laced: count−1, sizes, headers).
    pub fn from_codec_private(cp: &[u8]) -> Option<Self> {
        let headers = xiph_headers(cp)?;
        let ident = headers.first()?;
        let setup = headers.get(2)?;
        if ident.len() < 30 || ident[0] != 1 || &ident[1..7] != b"vorbis" {
            return None;
        }
        let blocksize = [1u32 << (ident[28] & 0x0F), 1u32 << (ident[28] >> 4)];
        if setup.first() != Some(&5) {
            return None;
        }
        let mode_long = setup_mode_blockflags(setup)?;
        let n = mode_long.len() as u32;
        let mode_bits = if n <= 1 { 0 } else { 32 - (n - 1).leading_zeros() };
        Some(VorbisBlocks {
            blocksize,
            mode_long,
            mode_bits,
            prev: None,
        })
    }

    /// Samples the decoder outputs for this packet (0 for the first one).
    pub fn packet_samples(&mut self, p: &[u8]) -> u32 {
        let Some(&b0) = p.first() else { return 0 };
        if b0 & 1 != 0 {
            return 0; // header packet
        }
        // Bit 0 = packet type, then the mode number (LSB-first).
        let mode = ((b0 as u32 >> 1) & ((1u32 << self.mode_bits) - 1)) as usize;
        let long = self.mode_long.get(mode).copied().unwrap_or(false);
        let cur = self.blocksize[long as usize];
        let out = match self.prev {
            None => 0,
            Some(prev) => {
                // A long block states its neighbour's size (previous window flag).
                let prev = if long {
                    let flag_bit = 1 + self.mode_bits;
                    let flag = read_bits_lsb(p, flag_bit, 1).unwrap_or(0);
                    self.blocksize[flag as usize]
                } else {
                    prev
                };
                (prev + cur) / 4
            }
        };
        self.prev = Some(cur);
        out
    }
}

/// Splits Xiph-laced headers: byte 0 = count − 1, then count − 1 sizes in
/// 255-continued form, then the headers back to back.
fn xiph_headers(cp: &[u8]) -> Option<Vec<&[u8]>> {
    let count = *cp.first()? as usize + 1;
    let mut p = 1;
    let mut sizes = Vec::with_capacity(count);
    for _ in 0..count - 1 {
        let mut n = 0usize;
        loop {
            let b = *cp.get(p)?;
            p += 1;
            n += b as usize;
            if b != 255 {
                break;
            }
        }
        sizes.push(n);
    }
    let mut out = Vec::with_capacity(count);
    for n in sizes {
        out.push(cp.get(p..p + n)?);
        p += n;
    }
    out.push(cp.get(p..)?);
    Some(out)
}

/// Bits `[pos, pos + n)` of an LSB-first bitstream (Vorbis packing).
fn read_bits_lsb(b: &[u8], pos: u32, n: u32) -> Option<u32> {
    let mut v = 0u32;
    for i in 0..n {
        let k = pos + i;
        let byte = *b.get((k / 8) as usize)?;
        v |= (((byte >> (k % 8)) & 1) as u32) << i;
    }
    Some(v)
}

/// The block flag of every mode, read backwards from the setup header's
/// framing bit: each mode is blockflag(1) windowtype(16) transformtype(16)
/// mapping(8), preceded by mode_count − 1 (6 bits). Window and transform
/// types are always 0, which makes the records recognisable from the end
/// (the same approach as FFmpeg's and liboggz's parsers; the largest count
/// consistent with its own 6-bit field wins).
fn setup_mode_blockflags(setup: &[u8]) -> Option<Vec<bool>> {
    let total = setup.len() as u32 * 8;
    let framing = (0..total)
        .rev()
        .find(|&k| read_bits_lsb(setup, k, 1) == Some(1))?;
    let mut best = None;
    for m in 1..=64u32 {
        let Some(start) = framing.checked_sub(41 * m) else {
            break;
        };
        let window = read_bits_lsb(setup, start + 1, 16)?;
        let transform = read_bits_lsb(setup, start + 17, 16)?;
        let mapping = read_bits_lsb(setup, start + 33, 8)?;
        if window != 0 || transform != 0 || mapping > 63 {
            break;
        }
        if let Some(cs) = start.checked_sub(6) {
            if read_bits_lsb(setup, cs, 6)? + 1 == m {
                best = Some(m);
            }
        }
    }
    let n = best?;
    Some(
        (0..n)
            .map(|i| read_bits_lsb(setup, framing - 41 * (n - i), 1) == Some(1))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ebml_sizes() {
        let mut v = Vec::new();
        vint_size(&mut v, Some(5));
        assert_eq!(v, [0x85]);
        v.clear();
        vint_size(&mut v, Some(127)); // 0x7F is reserved in one byte
        assert_eq!(v, [0x40, 0x7F]);
        v.clear();
        vint_size(&mut v, Some(300));
        assert_eq!(v, [0x41, 0x2C]);
    }

    #[test]
    fn vorbis_mode_scan() {
        // Two modes (short, long), mapping 0 and 1, then the framing bit.
        let mut bits: Vec<u8> = Vec::new();
        let mut push = |v: u32, n: u32| {
            for i in 0..n {
                bits.push(((v >> i) & 1) as u8);
            }
        };
        push(0b1010_1010, 8); // tail of the mapping configs
        push(1, 6); // mode_count − 1
        for (flag, map) in [(0, 0), (1, 1)] {
            push(flag, 1);
            push(0, 16);
            push(0, 16);
            push(map, 8);
        }
        push(1, 1); // framing
        let mut bytes = vec![0u8; bits.len().div_ceil(8)];
        for (k, b) in bits.iter().enumerate() {
            bytes[k / 8] |= b << (k % 8);
        }
        assert_eq!(setup_mode_blockflags(&bytes), Some(vec![false, true]));
    }

    #[test]
    fn vorbis_packet_durations() {
        let mut v = VorbisBlocks {
            blocksize: [256, 2048],
            mode_long: vec![false, true],
            mode_bits: 1,
            prev: None,
        };
        // mode 1 (long): bit0 = 0, bit1 = 1; previous window flag bit2 = 1.
        assert_eq!(v.packet_samples(&[0b0000_0110]), 0);
        assert_eq!(v.packet_samples(&[0b0000_0110]), 1024);
        // mode 0 (short) after long: (2048 + 256) / 4.
        assert_eq!(v.packet_samples(&[0b0000_0000]), 576);
    }
}
