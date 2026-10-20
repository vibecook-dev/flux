//! Audio elementary streams in MPEG-TS: framing of AAC (ADTS), AC-3/E-AC-3
//! and MPEG audio PES payloads into timed frames, and the stream facts the
//! index records (the same channel counts and rates flux-probe reports).

use bytes::Bytes;
use flux_core::Codec;

use crate::codec::audio::{self as caudio, Ac3Frame};

/// Codecs the TS remux frames itself; other audio is read by ffmpeg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// AAC in ADTS (stream type 0x0F, or private data that sniffs as ADTS).
    Adts,
    /// AC-3 and E-AC-3 sync frames.
    Ac3,
    /// MPEG-1/2 audio layers I–III.
    Mpa,
}

impl Framing {
    /// The framing of a track, from its codec and stream type tag
    /// (`"0x11"` is AAC in LATM/LOAS, which is not ADTS).
    pub(crate) fn of(codec: Codec, stream_type_tag: &str) -> Option<Framing> {
        match codec {
            Codec::Aac if stream_type_tag != "0x11" => Some(Framing::Adts),
            Codec::Ac3 | Codec::Eac3 => Some(Framing::Ac3),
            Codec::Mp2 | Codec::Mp3 => Some(Framing::Mpa),
            _ => None,
        }
    }
}

/// One frame header: (frame bytes, header bytes to drop, samples, rate,
/// dependent E-AC-3 substream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub len: usize,
    pub skip: usize,
    pub samples: u32,
    pub rate: u32,
    pub dependent: bool,
}

const AAC_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// ADTS header (ISO/IEC 14496-3 §1.A.2.2.1). Frames with several raw data
/// blocks are not supported (one fMP4 sample is one raw data block).
pub(crate) fn adts(b: &[u8]) -> Option<Header> {
    if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
        return None;
    }
    let rate = *AAC_RATES.get(((b[2] >> 2) & 0x0F) as usize)?;
    let len = (((b[3] & 0x03) as usize) << 11) | ((b[4] as usize) << 3) | ((b[5] as usize) >> 5);
    let skip = if b[1] & 1 == 1 { 7 } else { 9 };
    if len <= skip || b[6] & 0x03 != 0 {
        return None;
    }
    Some(Header {
        len,
        skip,
        samples: 1024,
        rate,
        dependent: false,
    })
}

/// AudioSpecificConfig (2 bytes) implied by an ADTS header.
pub(crate) fn adts_asc(b: &[u8]) -> Option<Vec<u8>> {
    adts(b)?;
    let profile = (b[2] >> 6) + 1;
    let fi = (b[2] >> 2) & 0x0F;
    let ch = ((b[2] & 1) << 2) | (b[3] >> 6);
    Some(vec![(profile << 3) | (fi >> 1), ((fi & 1) << 7) | (ch << 3)])
}

fn ac3(b: &[u8]) -> Option<Header> {
    let f = Ac3Frame::parse(b)?;
    (f.frame_size > 0).then_some(Header {
        len: f.frame_size,
        skip: 0,
        samples: f.samples,
        rate: f.sample_rate,
        dependent: f.eac3 && f.strmtyp == 1,
    })
}

/// MPEG audio frame header (ISO/IEC 11172-3 §2.4.2.3, 13818-3).
pub(crate) fn mpa(b: &[u8]) -> Option<Header> {
    if b.len() < 4 || b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (b[1] >> 3) & 3; // 0 = 2.5, 2 = 2, 3 = 1
    let layer = (b[1] >> 1) & 3; // 1 = III, 2 = II, 3 = I
    let bi = (b[2] >> 4) as usize;
    let si = ((b[2] >> 2) & 3) as usize;
    let pad = ((b[2] >> 1) & 1) as usize;
    if version == 1 || layer == 0 || bi == 0 || bi == 15 || si == 3 {
        return None;
    }
    const V1: [[u16; 15]; 3] = [
        [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
    ];
    const V2: [[u16; 15]; 2] = [
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ];
    let rate = match version {
        3 => [44100, 48000, 32000][si],
        2 => [22050, 24000, 16000][si],
        _ => [11025, 12000, 8000][si],
    };
    let kbps = match (version, layer) {
        (3, 3) => V1[0][bi],
        (3, 2) => V1[1][bi],
        (3, _) => V1[2][bi],
        (_, 3) => V2[0][bi],
        _ => V2[1][bi],
    } as usize
        * 1000;
    let r = rate as usize;
    let (len, samples) = match layer {
        3 => ((12 * kbps / r + pad) * 4, 384),
        2 => (144 * kbps / r + pad, 1152),
        _ if version == 3 => (144 * kbps / r + pad, 1152),
        _ => (72 * kbps / r + pad, 576),
    };
    Some(Header {
        len,
        skip: 0,
        samples,
        rate,
        dependent: false,
    })
}

impl Framing {
    fn header(self, b: &[u8]) -> Option<Header> {
        match self {
            Framing::Adts => adts(b),
            Framing::Ac3 => ac3(b),
            Framing::Mpa => mpa(b),
        }
    }

    fn is_sync(self, b: &[u8]) -> bool {
        match self {
            Framing::Adts => b.len() >= 2 && b[0] == 0xFF && b[1] & 0xF6 == 0xF0,
            Framing::Ac3 => b.len() >= 2 && b[0] == 0x0B && b[1] == 0x77,
            Framing::Mpa => b.len() >= 2 && b[0] == 0xFF && b[1] & 0xE0 == 0xE0,
        }
    }
}

/// One timed audio frame (for AC-3/E-AC-3: an independent sync frame with
/// the dependent substream frames that follow it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    /// Presentation time on the asset timeline, ns.
    pub pts_ns: i64,
    pub samples: u32,
    pub rate: u32,
    /// Raw AAC (ADTS header removed), or whole sync frames.
    pub data: Bytes,
    /// File offset of the PES the frame started in.
    pub pes_pos: u64,
}

impl Frame {
    pub(crate) fn duration_ns(&self) -> i64 {
        self.samples as i64 * 1_000_000_000 / self.rate.max(1) as i64
    }
}

/// Splits the PES payloads of one audio PID into timed frames. A PES
/// timestamp belongs to the first frame that *starts* in that PES (§2.4.3.7);
/// later frames, and frames continued from the previous PES, follow on.
pub(crate) struct Framer {
    framing: Framing,
    carry: Vec<u8>,
    /// Where the carried bytes' frame started.
    carry_pos: u64,
    /// Time of the next frame when no PES timestamp applies, ns.
    next_ns: Option<i64>,
    /// E-AC-3: the independent frame waiting for its dependent substreams.
    pending: Option<Frame>,
    /// The input position is known to be at a frame start.
    aligned: bool,
    /// Bytes skipped to regain sync.
    pub skipped: u64,
}

impl Framer {
    pub(crate) fn new(framing: Framing) -> Self {
        Framer {
            framing,
            carry: Vec::new(),
            carry_pos: 0,
            next_ns: None,
            pending: None,
            aligned: false,
            skipped: 0,
        }
    }

    /// Frames from one PES payload at file offset `pos`, whose timestamp
    /// on the asset timeline is `pts_ns`.
    pub(crate) fn push(&mut self, payload: &[u8], pos: u64, pts_ns: Option<i64>, out: &mut Vec<Frame>) {
        let carried = self.carry.len();
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(payload);
        let mut p = 0usize;
        let mut pes_pts = pts_ns;
        while p + 4 <= buf.len() {
            let Some(h) = self.framing.header(&buf[p..]) else {
                p += 1;
                self.skipped += 1;
                self.aligned = false;
                continue;
            };
            // After lost sync, a header counts once the next one follows it
            // (frame payloads can contain sync-like bytes).
            if !self.aligned {
                let next = p + h.len;
                if next + 2 <= buf.len() && !self.framing.is_sync(&buf[next..]) {
                    p += 1;
                    self.skipped += 1;
                    continue;
                }
            }
            if p + h.len > buf.len() {
                break;
            }
            self.aligned = true;
            let starts_here = p >= carried;
            let t = match (starts_here, pes_pts) {
                (true, Some(t)) => {
                    pes_pts = None;
                    Some(t)
                }
                _ => self.next_ns,
            };
            let data = Bytes::copy_from_slice(&buf[p + h.skip..p + h.len]);
            let fpos = if starts_here { pos } else { self.carry_pos };
            p += h.len;
            if h.dependent {
                if let Some(f) = self.pending.as_mut() {
                    let mut d = f.data.to_vec();
                    d.extend_from_slice(&data);
                    f.data = Bytes::from(d);
                }
                continue;
            }
            let Some(t) = t else {
                continue; // no time known yet: unusable
            };
            let f = Frame {
                pts_ns: t,
                samples: h.samples,
                rate: h.rate,
                data,
                pes_pos: fpos,
            };
            self.next_ns = Some(t + f.duration_ns());
            if let Some(prev) = self.pending.replace(f) {
                out.push(prev);
            }
        }
        // A frame continued in the next PES keeps its start position.
        if p < carried {
            // (still inside the carried bytes: same start)
        } else {
            self.carry_pos = pos;
        }
        if buf.len() - p > 64 << 10 {
            self.skipped += (buf.len() - p) as u64;
            p = buf.len();
        }
        self.carry = buf.split_off(p);
    }

    /// The last frame (call at the end of the input).
    pub(crate) fn finish(&mut self, out: &mut Vec<Frame>) {
        if let Some(f) = self.pending.take() {
            out.push(f);
        }
    }
}

// ---------------------------------------------------------------- facts

/// Audio facts from the first payload bytes of a stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AudioFacts {
    /// Refined codec (MPEG audio layer, AC-3 vs E-AC-3, sniffed ADTS…).
    pub codec: Option<Codec>,
    pub channels: Option<u8>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub profile: Option<String>,
}

fn find<T>(d: &[u8], f: impl Fn(&[u8]) -> Option<T>) -> Option<T> {
    (0..d.len().min(4096)).find_map(|i| f(&d[i..]))
}

fn adts_facts(b: &[u8]) -> Option<AudioFacts> {
    let h = adts(b)?;
    let profile = (b[2] >> 6) + 1;
    let cc = ((b[2] & 1) << 2) | (b[3] >> 6);
    Some(AudioFacts {
        codec: Some(Codec::Aac),
        channels: match cc {
            1..=6 => Some(cc),
            7 => Some(8),
            _ => None,
        },
        sample_rate: Some(h.rate),
        bit_depth: None,
        profile: match profile {
            1 => Some("Main".into()),
            2 => Some("LC".into()),
            3 => Some("SSR".into()),
            _ => None,
        },
    })
}

fn ac3_facts(b: &[u8]) -> Option<AudioFacts> {
    let f = Ac3Frame::parse(b)?;
    Some(AudioFacts {
        codec: Some(if f.eac3 { Codec::Eac3 } else { Codec::Ac3 }),
        channels: Some(f.channels()),
        sample_rate: Some(f.sample_rate),
        ..AudioFacts::default()
    })
}

fn mpa_facts(b: &[u8]) -> Option<AudioFacts> {
    let h = mpa(b)?;
    let layer = (b[1] >> 1) & 3;
    let mode = b[3] >> 6;
    Some(AudioFacts {
        codec: Some(match layer {
            1 => Codec::Mp3,
            2 => Codec::Mp2,
            _ => Codec::Other,
        }),
        channels: Some(if mode == 3 { 1 } else { 2 }),
        sample_rate: Some(h.rate),
        ..AudioFacts::default()
    })
}

fn dts_facts(b: &[u8]) -> Option<AudioFacts> {
    if b.len() < 12 || b[..4] != [0x7F, 0xFE, 0x80, 0x01] {
        return None;
    }
    let mut r = crate::bits::BitReader::new(&b[4..]);
    r.skip(1 + 5 + 1 + 7 + 14)?;
    let amode = r.u8(6)?;
    let sfreq = r.u8(4)?;
    r.skip(5 + 1 + 1 + 1 + 1 + 1 + 3 + 1 + 1)?;
    let lff = r.u8(2)?;
    const CH: [u8; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];
    const SR: [u32; 16] = [
        0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 0, 0,
    ];
    Some(AudioFacts {
        codec: Some(Codec::Dts),
        channels: CH.get(amode as usize).map(|c| c + u8::from(lff == 1 || lff == 2)),
        sample_rate: SR.get(sfreq as usize).copied().filter(|r| *r > 0),
        ..AudioFacts::default()
    })
}

fn truehd_facts(b: &[u8]) -> Option<AudioFacts> {
    let pos = b
        .windows(4)
        .take(64 * 1024)
        .position(|w| w == [0xF8, 0x72, 0x6F, 0xBA])?;
    let mut r = crate::bits::BitReader::new(b.get(pos + 4..)?);
    let ratebits = r.u32(4)?;
    r.skip(8)?;
    let count = |arr: u32| -> u8 {
        const C: [u8; 13] = [2, 1, 1, 2, 2, 2, 2, 1, 1, 2, 2, 1, 1];
        (0..13).filter(|i| arr & (1 << i) != 0).map(|i| C[i]).sum()
    };
    let ch1 = count(r.u32(5)?);
    r.skip(2)?;
    let ch2 = count(r.u32(13)?);
    let rate = if ratebits == 0xF {
        0
    } else {
        (if ratebits & 8 != 0 { 44100 } else { 48000 }) << (ratebits & 7)
    };
    Some(AudioFacts {
        codec: Some(Codec::TrueHd),
        channels: Some(if ch2 > 0 { ch2 } else { ch1 }).filter(|c| *c > 0),
        sample_rate: Some(rate).filter(|r| *r > 0),
        ..AudioFacts::default()
    })
}

fn bd_lpcm_facts(b: &[u8]) -> Option<AudioFacts> {
    let (x, y) = (*b.get(2)?, *b.get(3)?);
    const CH: [u8; 16] = [0, 1, 0, 2, 3, 3, 4, 4, 5, 6, 7, 8, 0, 0, 0, 0];
    let rate = match x & 0xF {
        1 => 48000,
        4 => 96000,
        5 => 192000,
        _ => 0,
    };
    let depth = match y >> 6 {
        1 => 16,
        2 => 20,
        3 => 24,
        _ => 0,
    };
    Some(AudioFacts {
        codec: Some(Codec::Pcm),
        channels: Some(CH[(x >> 4) as usize]).filter(|c| *c > 0),
        sample_rate: Some(rate).filter(|r| *r > 0),
        bit_depth: Some(depth).filter(|d| *d > 0),
        ..AudioFacts::default()
    })
}

/// Facts of an audio stream of `codec` from its first payload bytes, as
/// flux-probe derives them (`None` codec = unidentified private data).
pub(crate) fn facts(codec: Codec, d: &[u8]) -> Option<AudioFacts> {
    match codec {
        Codec::Aac => find(d, adts_facts),
        Codec::Ac3 | Codec::Eac3 => find(d, ac3_facts),
        Codec::Dts => find(d, dts_facts),
        Codec::TrueHd => truehd_facts(d),
        Codec::Mp2 | Codec::Mp3 => find(d, mpa_facts),
        Codec::Pcm => bd_lpcm_facts(d),
        // Private data with no identifying descriptor: sniff the payload.
        Codec::Other => find(d, |b| match b {
            [0x0B, 0x77, ..] => ac3_facts(b),
            [0x7F, 0xFE, 0x80, 0x01, ..] => dts_facts(b),
            [0xFF, x, ..] if x & 0xF6 == 0xF0 => adts_facts(b),
            [0xFF, x, ..] if x & 0xE0 == 0xE0 => mpa_facts(b),
            _ => None,
        }),
        _ => None,
    }
}

/// `dac3` / `dec3` body from the first access unit of an AC-3/E-AC-3 stream.
pub(crate) fn ac3_config(d: &[u8]) -> Option<(Vec<u8>, u32, u32, u64)> {
    let start = (0..d.len().min(4096)).find(|&i| Ac3Frame::parse(&d[i..]).is_some())?;
    let frames: Vec<Ac3Frame> = caudio::split_ac3(&d[start..]).into_iter().map(|x| x.0).collect();
    let f0 = *frames.first()?;
    if f0.eac3 {
        // One access unit: the independent frame and its dependents.
        let au: Vec<Ac3Frame> = std::iter::once(f0)
            .chain(frames.iter().skip(1).take_while(|f| f.strmtyp == 1).copied())
            .collect();
        let bytes: usize = au.iter().map(|f| f.frame_size).sum();
        let rate = bytes as u64 * 8 * f0.sample_rate as u64 / f0.samples.max(1) as u64;
        Some((caudio::dec3(&au)?, f0.samples, f0.sample_rate, rate))
    } else {
        let rate = f0.frame_size as u64 * 8 * f0.sample_rate as u64 / 1536;
        Some((caudio::dac3(&f0).to_vec(), f0.samples, f0.sample_rate, rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adts_frame(payload: &[u8]) -> Vec<u8> {
        let c = caudio::AacConfig {
            object_type: 2,
            sample_rate: 48000,
            channel_config: 2,
            frame_samples: 1024,
        };
        let len = payload.len() + 7;
        let fi = 3u8;
        let mut v = vec![
            0xFF,
            0xF1,
            (1 << 6) | (fi << 2) | (c.channel_config >> 2),
            ((c.channel_config & 3) << 6) | ((len >> 11) as u8 & 3),
            (len >> 3) as u8,
            (((len & 7) as u8) << 5) | 0x1F,
            0xFC,
        ];
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn adts_frames_split_across_pes_keep_their_times() {
        let f: Vec<Vec<u8>> = (0..4u8).map(|i| adts_frame(&[i; 10])).collect();
        let all: Vec<u8> = f.concat();
        assert_eq!(adts_asc(&all), Some(vec![0x11, 0x90]));
        let mut fr = Framer::new(Framing::Adts);
        let mut out = Vec::new();
        // PES 1: frames 0, 1 and half of 2; PES 2 (pts of frame 3): the rest.
        let cut = 17 * 2 + 8;
        fr.push(&all[..cut], 100, Some(1_000_000_000), &mut out);
        fr.push(&all[cut..], 200, Some(1_064_000_000), &mut out);
        fr.finish(&mut out);
        let times: Vec<i64> = out.iter().map(|f| f.pts_ns).collect();
        let step = 1024 * 1_000_000_000 / 48000;
        assert_eq!(
            times,
            vec![
                1_000_000_000,
                1_000_000_000 + step,
                1_000_000_000 + 2 * step,
                1_064_000_000
            ]
        );
        assert_eq!(out[2].data.as_ref(), &[2u8; 10]);
        assert_eq!(
            out.iter().map(|f| f.pes_pos).collect::<Vec<_>>(),
            vec![100, 100, 100, 200]
        );
    }

    #[test]
    fn mpeg_audio_frame_sizes() {
        // MPEG-1 layer II, 192 kb/s, 48 kHz: 144 * 192000 / 48000 = 576 bytes.
        let h = mpa(&[0xFF, 0xFD, 0xA4, 0x00]).unwrap();
        assert_eq!((h.len, h.samples, h.rate), (576, 1152, 48000));
        // MPEG-1 layer III, 128 kb/s, 44.1 kHz, padded: 417 + 1.
        let h = mpa(&[0xFF, 0xFB, 0x92, 0x00]).unwrap();
        assert_eq!((h.len, h.samples, h.rate), (418, 1152, 44100));
        assert_eq!(
            facts(Codec::Mp2, &[0, 0xFF, 0xFD, 0xA4, 0xC0]).and_then(|f| f.codec),
            Some(Codec::Mp2)
        );
    }
}
