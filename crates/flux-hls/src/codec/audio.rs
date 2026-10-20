//! Audio bitstream helpers: just enough header parsing to build ISO sample
//! entries (`esds`, `dac3`, `dec3`, `dOps`, `dfLa`) and to know each frame's
//! duration in samples.

use crate::bits::BitReader;

// ------------------------------------------------------------------ AAC

const AAC_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Facts from an MPEG-4 AudioSpecificConfig (ISO/IEC 14496-3 §1.6.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacConfig {
    /// Audio object type (2 = LC, 5 = SBR/HE-AAC, 29 = PS/HE-AACv2).
    pub object_type: u8,
    pub sample_rate: u32,
    pub channel_config: u8,
    /// 1024, or 960 when `frameLengthFlag` is set.
    pub frame_samples: u32,
}

impl AacConfig {
    pub fn parse(asc: &[u8]) -> Option<Self> {
        let mut r = BitReader::new(asc);
        let mut object_type = r.u8(5)?;
        if object_type == 31 {
            object_type = 32 + r.u8(6)?;
        }
        let fi = r.u8(4)?;
        let sample_rate = if fi == 15 {
            r.u32(24)?
        } else {
            *AAC_RATES.get(fi as usize)?
        };
        let channel_config = r.u8(4)?;
        let mut frame_samples = 1024;
        if matches!(object_type, 5 | 29) {
            // Explicit SBR: extension sampling frequency then the core AOT.
            let efi = r.u8(4)?;
            if efi == 15 {
                r.skip(24)?;
            }
            let core = r.u8(5)?;
            if matches!(core, 1 | 2 | 3 | 4 | 6 | 7 | 17 | 19 | 20 | 21 | 22 | 23) && r.bit()? {
                frame_samples = 960;
            }
        } else if matches!(object_type, 1 | 2 | 3 | 4 | 6 | 7 | 17 | 19 | 20 | 21 | 22 | 23) && r.bit()? {
            frame_samples = 960;
        }
        Some(AacConfig {
            object_type,
            sample_rate,
            channel_config,
            frame_samples,
        })
    }

    /// RFC 6381 codec string: `mp4a.40.<aot>`.
    pub fn codec_string(&self) -> String {
        format!("mp4a.40.{}", self.object_type)
    }
}

/// Synthesizes a 2-byte AAC-LC AudioSpecificConfig.
pub fn aac_lc_asc(sample_rate: u32, channels: u8) -> Vec<u8> {
    let fi = AAC_RATES.iter().position(|&r| r == sample_rate).unwrap_or(3) as u8;
    let ch = channels.min(7);
    vec![(2 << 3) | (fi >> 1), ((fi & 1) << 7) | (ch << 3)]
}

/// Splits an ADTS stream into raw AAC frames. Returns the frames and the
/// AudioSpecificConfig implied by the first header.
pub fn split_adts(data: &[u8]) -> (Vec<&[u8]>, Option<Vec<u8>>) {
    let mut frames = Vec::new();
    let mut asc = None;
    let mut p = 0usize;
    while p + 7 <= data.len() {
        if data[p] != 0xFF || data[p + 1] & 0xF6 != 0xF0 {
            p += 1;
            continue;
        }
        let protection_absent = data[p + 1] & 1 == 1;
        let profile = (data[p + 2] >> 6) + 1;
        let fi = (data[p + 2] >> 2) & 0x0F;
        let ch = ((data[p + 2] & 1) << 2) | (data[p + 3] >> 6);
        let len = (((data[p + 3] & 0x03) as usize) << 11)
            | ((data[p + 4] as usize) << 3)
            | ((data[p + 5] as usize) >> 5);
        let header = if protection_absent { 7 } else { 9 };
        if len < header || p + len > data.len() {
            break;
        }
        if asc.is_none() {
            asc = Some(vec![(profile << 3) | (fi >> 1), ((fi & 1) << 7) | (ch << 3)]);
        }
        frames.push(&data[p + header..p + len]);
        p += len;
    }
    (frames, asc)
}

// ------------------------------------------------------------------ AC-3 / E-AC-3

/// AC-3 / E-AC-3 sync frame facts (ATSC A/52:2018 §5.4.1 and Annex E §E.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ac3Frame {
    pub eac3: bool,
    pub fscod: u8,
    pub bsid: u8,
    pub bsmod: u8,
    pub acmod: u8,
    pub lfeon: bool,
    /// AC-3 only: frmsizecod >> 1.
    pub bit_rate_code: u8,
    /// Frame size in bytes.
    pub frame_size: usize,
    pub sample_rate: u32,
    /// Samples per frame (1536 for AC-3; 256 × blocks for E-AC-3).
    pub samples: u32,
    /// E-AC-3: 0 independent, 1 dependent, 2 AC-3 convert.
    pub strmtyp: u8,
    /// E-AC-3 dependent substream channel map, when present.
    pub chanmap: Option<u16>,
}

const AC3_FRAME_WORDS_48K: [u16; 19] = [
    64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 640, 768, 896, 1024, 1152, 1280,
];

impl Ac3Frame {
    pub fn channels(&self) -> u8 {
        const CH: [u8; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
        CH[self.acmod as usize & 7] + self.lfeon as u8
    }

    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 8 || b[0] != 0x0B || b[1] != 0x77 {
            return None;
        }
        let bsid = b[5] >> 3;
        if bsid <= 10 {
            // AC-3
            let mut r = BitReader::new(&b[4..]);
            let fscod = r.u8(2)?;
            let frmsizecod = r.u8(6)?;
            let bsid = r.u8(5)?;
            let bsmod = r.u8(3)?;
            let acmod = r.u8(3)?;
            if acmod & 1 != 0 && acmod != 1 {
                r.skip(2)?;
            }
            if acmod & 4 != 0 {
                r.skip(2)?;
            }
            if acmod == 2 {
                r.skip(2)?;
            }
            let lfeon = r.bit()?;
            let sample_rate = [48000, 44100, 32000].get(fscod as usize).copied()?;
            let words48 = *AC3_FRAME_WORDS_48K.get((frmsizecod >> 1) as usize)? as usize;
            let words = match fscod {
                0 => words48,
                1 => (words48 * 48000 / 44100) + (frmsizecod as usize & 1),
                _ => words48 * 3 / 2,
            };
            Some(Ac3Frame {
                eac3: false,
                fscod,
                bsid,
                bsmod,
                acmod,
                lfeon,
                bit_rate_code: frmsizecod >> 1,
                frame_size: words * 2,
                sample_rate,
                samples: 1536,
                strmtyp: 0,
                chanmap: None,
            })
        } else if bsid <= 16 {
            // E-AC-3 (Annex E)
            let mut r = BitReader::new(&b[2..]);
            let strmtyp = r.u8(2)?;
            r.skip(3)?; // substreamid
            let frmsiz = r.u16(11)? as usize;
            let fscod = r.u8(2)?;
            let (sample_rate, blocks) = if fscod == 3 {
                let fscod2 = r.u8(2)?;
                ([24000, 22050, 16000].get(fscod2 as usize).copied()?, 6)
            } else {
                let numblkscod = r.u8(2)?;
                (
                    [48000, 44100, 32000][fscod as usize],
                    [1, 2, 3, 6][numblkscod as usize],
                )
            };
            let acmod = r.u8(3)?;
            let lfeon = r.bit()?;
            let bsid = r.u8(5)?;
            r.skip(5)?; // dialnorm
            if r.bit()? {
                r.skip(8)?; // compr
            }
            if acmod == 0 {
                r.skip(5)?;
                if r.bit()? {
                    r.skip(8)?;
                }
            }
            let mut chanmap = None;
            if strmtyp == 1 && r.bit()? {
                chanmap = Some(r.u16(16)?);
            }
            Some(Ac3Frame {
                eac3: true,
                fscod,
                bsid,
                bsmod: 0,
                acmod,
                lfeon,
                bit_rate_code: 0,
                frame_size: (frmsiz + 1) * 2,
                sample_rate,
                samples: blocks * 256,
                strmtyp,
                chanmap,
            })
        } else {
            None
        }
    }
}

/// `dac3` box body (ETSI TS 102 366 Annex F.4).
pub fn dac3(f: &Ac3Frame) -> [u8; 3] {
    let v: u32 = ((f.fscod as u32) << 22)
        | ((f.bsid as u32) << 17)
        | ((f.bsmod as u32) << 14)
        | ((f.acmod as u32) << 11)
        | ((f.lfeon as u32) << 10)
        | ((f.bit_rate_code as u32) << 5);
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// `dec3` box body (ETSI TS 102 366 Annex F.6) for one independent substream,
/// built from all sync frames of one access unit (independent + dependent).
pub fn dec3(frames: &[Ac3Frame]) -> Option<Vec<u8>> {
    let ind = frames.iter().find(|f| f.strmtyp != 1)?;
    let deps: Vec<&Ac3Frame> = frames.iter().filter(|f| f.strmtyp == 1).collect();
    let total_bytes: usize = frames.iter().map(|f| f.frame_size).sum();
    let data_rate =
        (total_bytes as u64 * 8 * ind.sample_rate as u64 / ind.samples.max(1) as u64 / 1000) as u32;
    let mut bits: Vec<(u32, u32)> = vec![(data_rate.min(0x1FFF), 13), (0, 3)]; // num_ind_sub - 1 = 0
    bits.push((ind.fscod as u32, 2));
    bits.push((ind.bsid as u32, 5));
    bits.push((0, 1)); // reserved
    bits.push((0, 1)); // asvc
    bits.push((ind.bsmod as u32, 3));
    bits.push((ind.acmod as u32, 3));
    bits.push((ind.lfeon as u32, 1));
    bits.push((0, 3)); // reserved
    bits.push((deps.len().min(15) as u32, 4));
    if deps.is_empty() {
        bits.push((0, 1));
    } else {
        // chan_loc: the dependent substream's channel map shifted into the
        // 9-bit dec3 layout. Unverified: the corpus has no 7.1 E-AC-3 with
        // dependent substreams.
        let chan_loc = deps
            .iter()
            .filter_map(|d| d.chanmap)
            .fold(0u32, |a, m| a | ((m as u32 >> 5) & 0x1FF));
        bits.push((chan_loc, 9));
    }
    let mut out = Vec::new();
    let mut acc = 0u64;
    let mut n = 0u32;
    for (v, w) in bits {
        acc = (acc << w) | (v as u64 & ((1u64 << w) - 1));
        n += w;
        while n >= 8 {
            out.push((acc >> (n - 8)) as u8);
            n -= 8;
        }
    }
    if n > 0 {
        out.push((acc << (8 - n)) as u8);
    }
    Some(out)
}

/// Splits a buffer holding one or more AC-3/E-AC-3 sync frames.
pub fn split_ac3(mut data: &[u8]) -> Vec<(Ac3Frame, &[u8])> {
    let mut out = Vec::new();
    while let Some(f) = Ac3Frame::parse(data) {
        if f.frame_size == 0 || f.frame_size > data.len() {
            break;
        }
        out.push((f, &data[..f.frame_size]));
        data = &data[f.frame_size..];
    }
    out
}

// ------------------------------------------------------------------ Opus

/// Fields of an `OpusHead` (RFC 7845 §5.1), as stored in Matroska CodecPrivate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusHead {
    pub channels: u8,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    pub output_gain: i16,
    pub mapping_family: u8,
    /// stream count, coupled count, channel mapping (family != 0 only).
    pub mapping: Vec<u8>,
}

impl OpusHead {
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 19 || &b[..8] != b"OpusHead" {
            return None;
        }
        Some(OpusHead {
            channels: b[9],
            pre_skip: u16::from_le_bytes([b[10], b[11]]),
            input_sample_rate: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
            output_gain: i16::from_le_bytes([b[16], b[17]]),
            mapping_family: b[18],
            mapping: b.get(19..).unwrap_or_default().to_vec(),
        })
    }

    /// `dOps` box body (Opus in ISOBMFF §4.3.2). Note: big-endian, unlike OpusHead.
    pub fn dops(&self) -> Vec<u8> {
        let mut v = vec![0u8, self.channels];
        v.extend_from_slice(&self.pre_skip.to_be_bytes());
        v.extend_from_slice(&self.input_sample_rate.to_be_bytes());
        v.extend_from_slice(&self.output_gain.to_be_bytes());
        v.push(self.mapping_family);
        if self.mapping_family != 0 {
            v.extend_from_slice(&self.mapping);
        }
        v
    }
}

/// Samples (at 48 kHz) in one Opus packet, from its TOC (RFC 6716 §3.1).
pub fn opus_packet_samples(p: &[u8]) -> Option<u32> {
    let toc = *p.first()?;
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][(config % 4) as usize],
        12..=15 => [480, 960][(config % 2) as usize],
        _ => [120, 240, 480, 960][(config % 4) as usize],
    };
    let count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*p.get(1)? & 0x3F) as u32,
    };
    Some(frame * count)
}

// ------------------------------------------------------------------ FLAC

/// Builds the `dfLa` body (FLAC in ISOBMFF §3.3.2) from Matroska CodecPrivate
/// (`fLaC` + metadata blocks): keeps STREAMINFO only and marks it last.
pub fn dfla_from_codec_private(cp: &[u8]) -> Option<Vec<u8>> {
    let blocks = cp.strip_prefix(b"fLaC").unwrap_or(cp);
    if blocks.len() < 4 + 34 || blocks[0] & 0x7F != 0 {
        return None;
    }
    let len = u32::from_be_bytes([0, blocks[1], blocks[2], blocks[3]]) as usize;
    let mut out = Vec::with_capacity(4 + len);
    out.push(0x80); // last-metadata-block flag | type 0 (STREAMINFO)
    out.extend_from_slice(blocks.get(1..4 + len)?);
    Some(out)
}

/// STREAMINFO facts: (sample rate, channels, bits per sample).
pub fn flac_streaminfo(dfla: &[u8]) -> Option<(u32, u8, u8)> {
    let si = dfla.get(4..4 + 34)?;
    let mut r = BitReader::new(&si[10..]);
    let rate = r.u32(20)?;
    let ch = r.u8(3)? + 1;
    let bps = r.u8(5)? + 1;
    Some((rate, ch, bps))
}

/// Block size (samples) of a FLAC frame from its header (FLAC format §9.1).
pub fn flac_frame_samples(f: &[u8]) -> Option<u32> {
    if f.len() < 5 || f[0] != 0xFF || f[1] & 0xFE != 0xF8 {
        return None;
    }
    let code = f[2] >> 4;
    Some(match code {
        1 => 192,
        2..=5 => 576 << (code - 2),
        6 | 7 => {
            // The frame/sample number (UTF-8-like) precedes the explicit size.
            let first = f[4];
            let n = if first < 0x80 {
                1
            } else {
                first.leading_ones() as usize
            };
            let p = 4 + n;
            if code == 6 {
                *f.get(p)? as u32 + 1
            } else {
                u16::from_be_bytes([*f.get(p)?, *f.get(p + 1)?]) as u32 + 1
            }
        }
        8..=15 => 256 << (code - 8),
        _ => return None,
    })
}

// ------------------------------------------------------------------ DTS / TrueHD / MP3

/// Samples per DTS core frame: (NBLKS + 1) × 32 (ETSI TS 102 114 §5.3.1).
pub fn dts_frame_samples(f: &[u8]) -> Option<u32> {
    if f.len() < 10 || f[..4] != [0x7F, 0xFE, 0x80, 0x01] {
        return None;
    }
    let mut r = BitReader::new(&f[4..]);
    r.skip(1 + 5 + 1)?;
    Some((r.u32(7)? + 1) * 32)
}

/// True when a TrueHD/MLP access unit carries a major sync (a decoder can
/// start there).
pub fn truehd_is_major_sync(au: &[u8]) -> bool {
    au.len() >= 8 && au[4..7] == [0xF8, 0x72, 0x6F] && (au[7] == 0xBA || au[7] == 0xBB)
}

/// Samples per MPEG audio frame (Layer I/II/III) from its header.
pub fn mpeg_audio_frame_samples(f: &[u8]) -> Option<u32> {
    if f.len() < 4 || f[0] != 0xFF || f[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (f[1] >> 3) & 3; // 3 = MPEG-1
    let layer = (f[1] >> 1) & 3; // 1 = III, 2 = II, 3 = I
    Some(match (layer, version) {
        (3, _) => 384,
        (2, _) => 1152,
        (1, 3) => 1152,
        (1, _) => 576,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_lc_48k_stereo() {
        let asc = aac_lc_asc(48000, 2);
        assert_eq!(asc, vec![0x11, 0x90]);
        let c = AacConfig::parse(&asc).unwrap();
        assert_eq!(
            (c.object_type, c.sample_rate, c.channel_config, c.frame_samples),
            (2, 48000, 2, 1024)
        );
        assert_eq!(c.codec_string(), "mp4a.40.2");
    }

    #[test]
    fn opus_toc() {
        // config 31 (CELT FB 20 ms), code 0 -> 960 samples
        assert_eq!(opus_packet_samples(&[31 << 3]), Some(960));
        // config 1 (SILK NB 20ms), code 1 (two frames) -> 1920
        assert_eq!(opus_packet_samples(&[(1 << 3) | 1]), Some(1920));
    }

    #[test]
    fn dec3_single_substream() {
        let f = Ac3Frame {
            eac3: true,
            fscod: 0,
            bsid: 16,
            bsmod: 0,
            acmod: 7,
            lfeon: true,
            bit_rate_code: 0,
            frame_size: 2560,
            sample_rate: 48000,
            samples: 1536,
            strmtyp: 0,
            chanmap: None,
        };
        let d = dec3(&[f]).unwrap();
        // data_rate = 2560*8*48000/1536/1000 = 640 kb/s
        assert_eq!(((d[0] as u16) << 5) | (d[1] as u16 >> 3), 640);
        assert_eq!(d.len(), 5);
        assert_eq!(f.channels(), 6);
    }

    #[test]
    fn dac3_layout() {
        let f = Ac3Frame {
            eac3: false,
            fscod: 0,
            bsid: 8,
            bsmod: 0,
            acmod: 7,
            lfeon: true,
            bit_rate_code: 15,
            frame_size: 1792,
            sample_rate: 48000,
            samples: 1536,
            strmtyp: 0,
            chanmap: None,
        };
        // fscod 00 | bsid 01000 | bsmod 000 | acmod 111 | lfe 1 | brc 01111 | 00000
        assert_eq!(dac3(&f), [0x10, 0x3D, 0xE0]);
    }

    #[test]
    fn flac_block_sizes() {
        // blocksize code 0xC -> 4096
        assert_eq!(flac_frame_samples(&[0xFF, 0xF8, 0xC9, 0x08, 0x00]), Some(4096));
    }
}
