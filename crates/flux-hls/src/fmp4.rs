//! Hand-written ISO BMFF / CMAF writer (ISO/IEC 14496-12, ISO/IEC 23000-19).
//!
//! Init segment: `ftyp` (iso6, cmfc) + `moov` (mvhd, trak…, mvex/trex).
//! Media segment: `styp` + `moof` (mfhd, traf/tfhd default-base-is-moof,
//! tfdt v1, trun) + `mdat`. The mdat payload is *not* copied here: the writer
//! returns the header bytes and the caller streams the sample ranges.

use bytes::Bytes;
use flux_core::{ColorInfo, ContentLightLevel, MasteringDisplay};

use crate::body::{Part, SegmentBody};
use crate::index::DoviRecord;

/// Big-endian box writer with back-patched sizes.
pub(crate) struct W(pub Vec<u8>);

impl W {
    pub(crate) fn new() -> Self {
        W(Vec::with_capacity(1024))
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v)
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes())
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes())
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes())
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes())
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b)
    }
    fn zeros(&mut self, n: usize) {
        self.0.resize(self.0.len() + n, 0)
    }
    fn begin(&mut self, kind: &[u8; 4]) -> usize {
        let at = self.0.len();
        self.u32(0);
        self.bytes(kind);
        at
    }
    fn full(&mut self, kind: &[u8; 4], version: u8, flags: u32) -> usize {
        let at = self.begin(kind);
        self.u32(((version as u32) << 24) | (flags & 0xFF_FFFF));
        at
    }
    fn end(&mut self, at: usize) {
        let len = (self.0.len() - at) as u32;
        self.0[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }
    fn leaf(&mut self, kind: &[u8; 4], body: &[u8]) {
        let b = self.begin(kind);
        self.bytes(body);
        self.end(b);
    }
    fn matrix(&mut self) {
        self.matrix_of(&IDENTITY);
    }
    fn matrix_of(&mut self, m: &[i32; 9]) {
        for &v in m {
            self.i32(v);
        }
    }
}

/// The identity transformation matrix (a, b, u, c, d, v, x, y, w).
pub const IDENTITY: [i32; 9] = [0x1_0000, 0, 0, 0, 0x1_0000, 0, 0, 0, 0x4000_0000];

/// Audio decoder configuration carried by the sample entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioConfig {
    /// `esds` with an MPEG-4 ObjectTypeIndication (0x40 AAC, 0x6B MP3) and
    /// optional DecoderSpecificInfo (AudioSpecificConfig).
    Esds {
        object_type: u8,
        dsi: Vec<u8>,
        avg_bitrate: u32,
    },
    /// `dac3` body (3 bytes).
    Dac3(Vec<u8>),
    /// `dec3` body.
    Dec3(Vec<u8>),
    /// `dOps` body.
    Dops(Vec<u8>),
    /// `dfLa` metadata blocks (without the full-box header).
    Dfla(Vec<u8>),
}

/// Sample entry description for one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntrySpec {
    Video {
        /// `avc1`, `avc3`, `hvc1`, `hev1`, `av01`, `dvh1`…
        fourcc: [u8; 4],
        /// `avcC`, `hvcC` or `av1C`.
        config_box: [u8; 4],
        config: Vec<u8>,
        width: u16,
        height: u16,
        color: Option<ColorInfo>,
        mastering: Option<MasteringDisplay>,
        cll: Option<ContentLightLevel>,
        dovi: Option<DoviRecord>,
        /// Pixel aspect ratio h:v for `pasp` (anamorphic video); also widens
        /// the `tkhd` display width.
        pasp: Option<(u32, u32)>,
        /// `tkhd` transformation matrix (display rotation), when not identity.
        matrix: Option<[i32; 9]>,
    },
    Audio {
        /// `mp4a`, `ac-3`, `ec-3`, `Opus`, `fLaC`.
        fourcc: [u8; 4],
        channels: u16,
        sample_rate: u32,
        config: AudioConfig,
    },
}

/// One track of an init segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitSpec {
    pub track_id: u32,
    pub timescale: u32,
    /// Packed ISO 639-2 language for `mdhd`.
    pub language: u16,
    pub entry: EntrySpec,
}

fn write_desc(w: &mut W, tag: u8, body: &[u8]) {
    w.u8(tag);
    // 4-byte expandable length (always valid, simplest to emit).
    let n = body.len() as u32;
    w.u8(0x80 | ((n >> 21) & 0x7F) as u8);
    w.u8(0x80 | ((n >> 14) & 0x7F) as u8);
    w.u8(0x80 | ((n >> 7) & 0x7F) as u8);
    w.u8((n & 0x7F) as u8);
    w.bytes(body);
}

fn write_esds(w: &mut W, es_id: u16, object_type: u8, dsi: &[u8], avg_bitrate: u32) {
    let esds = w.full(b"esds", 0, 0);
    let mut dcd = W::new();
    dcd.u8(object_type);
    dcd.u8(0x15); // streamType 5 (audio) << 2 | upStream 0 | reserved 1
    dcd.bytes(&[0, 0, 0]); // bufferSizeDB
    dcd.u32(avg_bitrate); // maxBitrate (unknown: use average)
    dcd.u32(avg_bitrate);
    if !dsi.is_empty() {
        write_desc(&mut dcd, 0x05, dsi);
    }
    let mut es = W::new();
    es.u16(es_id);
    es.u8(0);
    write_desc(&mut es, 0x04, &dcd.0);
    write_desc(&mut es, 0x06, &[0x02]);
    write_desc(w, 0x03, &es.0);
    w.end(esds);
}

fn write_entry(w: &mut W, track_id: u32, e: &EntrySpec) {
    match e {
        EntrySpec::Video {
            fourcc,
            config_box,
            config,
            width,
            height,
            color,
            mastering,
            cll,
            dovi,
            pasp,
            ..
        } => {
            let se = w.begin(fourcc);
            w.zeros(6);
            w.u16(1); // data_reference_index
            w.zeros(16);
            w.u16(*width);
            w.u16(*height);
            w.u32(0x0048_0000);
            w.u32(0x0048_0000);
            w.u32(0);
            w.u16(1); // frame_count
            w.zeros(32); // compressorname
            w.u16(0x0018);
            w.u16(0xFFFF);
            // Matroska CodecPrivate for AVC/HEVC/AV1 *is* the ISO record: pass through.
            w.leaf(config_box, config);
            if let Some(c) = color {
                if c.primaries.is_some() || c.transfer.is_some() || c.matrix.is_some() {
                    let b = w.begin(b"colr");
                    w.bytes(b"nclx");
                    w.u16(c.primaries.unwrap_or(2) as u16);
                    w.u16(c.transfer.unwrap_or(2) as u16);
                    w.u16(c.matrix.unwrap_or(2) as u16);
                    w.u8(if c.full_range == Some(true) { 0x80 } else { 0 });
                    w.end(b);
                }
            }
            if let Some(m) = mastering {
                let b = w.begin(b"mdcv");
                for p in m.primaries {
                    w.u16(p[0]);
                    w.u16(p[1]);
                }
                w.u16(m.white_point[0]);
                w.u16(m.white_point[1]);
                w.u32(m.max_luminance);
                w.u32(m.min_luminance);
                w.end(b);
            }
            if let Some(c) = cll {
                let b = w.begin(b"clli");
                w.u16(c.max_cll);
                w.u16(c.max_fall);
                w.end(b);
            }
            if let Some(d) = dovi {
                w.leaf(&d.fourcc, &d.data);
            }
            if let Some((h, v)) = pasp.filter(|(h, v)| h != v && *h > 0 && *v > 0) {
                let b = w.begin(b"pasp");
                w.u32(h);
                w.u32(v);
                w.end(b);
            }
            w.end(se);
        }
        EntrySpec::Audio {
            fourcc,
            channels,
            sample_rate,
            config,
        } => {
            let se = w.begin(fourcc);
            w.zeros(6);
            w.u16(1);
            w.zeros(8);
            w.u16(*channels);
            w.u16(16);
            w.u16(0);
            w.u16(0);
            w.u32(if *sample_rate <= 0xFFFF {
                sample_rate << 16
            } else {
                0
            });
            match config {
                AudioConfig::Esds {
                    object_type,
                    dsi,
                    avg_bitrate,
                } => write_esds(w, track_id as u16, *object_type, dsi, *avg_bitrate),
                AudioConfig::Dac3(b) => w.leaf(b"dac3", b),
                AudioConfig::Dec3(b) => w.leaf(b"dec3", b),
                AudioConfig::Dops(b) => w.leaf(b"dOps", b),
                AudioConfig::Dfla(b) => {
                    let f = w.full(b"dfLa", 0, 0);
                    w.bytes(b);
                    w.end(f);
                }
            }
            w.end(se);
        }
    }
}

/// Writes a CMAF init segment (`ftyp` + `moov`) for the given tracks.
pub fn init_segment(tracks: &[InitSpec]) -> Vec<u8> {
    let mut w = W::new();
    let f = w.begin(b"ftyp");
    w.bytes(b"iso6");
    w.u32(0);
    for b in [b"iso6", b"cmfc", b"mp41"] {
        w.bytes(b);
    }
    w.end(f);
    let moov = w.begin(b"moov");
    let mvhd = w.full(b"mvhd", 0, 0);
    w.u32(0);
    w.u32(0);
    w.u32(1000);
    w.u32(0);
    w.u32(0x0001_0000);
    w.u16(0x0100);
    w.zeros(10);
    w.matrix();
    w.zeros(24);
    w.u32(tracks.iter().map(|t| t.track_id).max().unwrap_or(0) + 1);
    w.end(mvhd);
    for t in tracks {
        let is_video = matches!(t.entry, EntrySpec::Video { .. });
        // tkhd: the display size before the matrix (16.16): the coded size
        // with the pixel aspect applied to the width (ISO/IEC 14496-12
        // §8.3.2.3), and the source's rotation matrix.
        let (width, height, matrix) = match &t.entry {
            EntrySpec::Video {
                width,
                height,
                pasp,
                matrix,
                ..
            } => {
                let display_w = match pasp.filter(|(h, v)| *h > 0 && *v > 0) {
                    Some((h, v)) => (*width as u64 * h as u64 * 65536 / v as u64).min(u32::MAX as u64) as u32,
                    None => (*width as u32) << 16,
                };
                (display_w, (*height as u32) << 16, matrix.unwrap_or(IDENTITY))
            }
            _ => (0, 0, IDENTITY),
        };
        let trak = w.begin(b"trak");
        let tkhd = w.full(b"tkhd", 0, 3); // enabled | in_movie
        w.u32(0);
        w.u32(0);
        w.u32(t.track_id);
        w.u32(0);
        w.u32(0);
        w.zeros(8);
        w.u16(0);
        w.u16(0);
        w.u16(if is_video { 0 } else { 0x0100 });
        w.u16(0);
        w.matrix_of(&matrix);
        w.u32(width);
        w.u32(height);
        w.end(tkhd);
        let mdia = w.begin(b"mdia");
        let mdhd = w.full(b"mdhd", 0, 0);
        w.u32(0);
        w.u32(0);
        w.u32(t.timescale);
        w.u32(0);
        w.u16(t.language);
        w.u16(0);
        w.end(mdhd);
        let hdlr = w.full(b"hdlr", 0, 0);
        w.u32(0);
        w.bytes(if is_video { b"vide" } else { b"soun" });
        w.zeros(12);
        w.bytes(if is_video {
            b"flux video\0"
        } else {
            b"flux audio\0"
        });
        w.end(hdlr);
        let minf = w.begin(b"minf");
        if is_video {
            let v = w.full(b"vmhd", 0, 1);
            w.zeros(8);
            w.end(v);
        } else {
            let s = w.full(b"smhd", 0, 0);
            w.zeros(4);
            w.end(s);
        }
        let dinf = w.begin(b"dinf");
        let dref = w.full(b"dref", 0, 0);
        w.u32(1);
        let url = w.full(b"url ", 0, 1);
        w.end(url);
        w.end(dref);
        w.end(dinf);
        let stbl = w.begin(b"stbl");
        let stsd = w.full(b"stsd", 0, 0);
        w.u32(1);
        write_entry(&mut w, t.track_id, &t.entry);
        w.end(stsd);
        for k in [b"stts", b"stsc", b"stco"] {
            let b = w.full(k, 0, 0);
            w.u32(0);
            w.end(b);
        }
        let stsz = w.full(b"stsz", 0, 0);
        w.u32(0);
        w.u32(0);
        w.end(stsz);
        w.end(stbl);
        w.end(minf);
        w.end(mdia);
        w.end(trak);
    }
    let mvex = w.begin(b"mvex");
    for t in tracks {
        let trex = w.full(b"trex", 0, 0);
        w.u32(t.track_id);
        w.u32(1);
        w.u32(0);
        w.u32(0);
        w.u32(0);
        w.end(trex);
    }
    w.end(mvex);
    w.end(moov);
    w.0
}

/// Sync sample (`sample_depends_on` = 2).
pub(crate) const FLAG_SYNC: u32 = 0x0200_0000;
/// Non-sync sample (`sample_depends_on` = 1, `sample_is_non_sync_sample`).
pub(crate) const FLAG_NONSYNC: u32 = 0x0101_0000;

/// Where a sample's bytes come from.
#[derive(Debug, Clone)]
pub(crate) enum SampleData {
    /// A byte range of the source file.
    File { offset: u64, len: u32 },
    /// Bytes already in memory (zero-copy slice of a read window, or rewritten).
    Bytes(Bytes),
    /// Prefix bytes (Matroska header stripping) followed by a file range.
    Prefixed { prefix: Bytes, offset: u64, len: u32 },
}

impl SampleData {
    pub(crate) fn len(&self) -> u32 {
        match self {
            SampleData::File { len, .. } => *len,
            SampleData::Bytes(b) => b.len() as u32,
            SampleData::Prefixed { prefix, len, .. } => prefix.len() as u32 + *len,
        }
    }
}

/// One sample of a fragment, in the track's output timescale.
#[derive(Debug, Clone)]
pub(crate) struct Sample {
    pub duration: u32,
    /// Composition offset (PTS − DTS).
    pub cts: i32,
    pub sync: bool,
    pub data: SampleData,
}

/// One track run of a media segment.
#[derive(Debug, Clone)]
pub(crate) struct Fragment {
    pub track_id: u32,
    /// `tfdt` base media decode time.
    pub base_dts: u64,
    pub samples: Vec<Sample>,
    /// Write per-sample flags and composition offsets (video).
    pub video: bool,
}

/// Builds a media segment: header bytes (`styp`+`moof`+`mdat` header) plus
/// the sample payload parts in `trun` order.
pub(crate) fn media_segment(seq: u32, frags: &[Fragment]) -> SegmentBody {
    let mut w = W::new();
    let styp = w.begin(b"styp");
    w.bytes(b"msdh");
    w.u32(0);
    w.bytes(b"msdh");
    w.bytes(b"msix");
    w.end(styp);
    let moof_at = w.0.len();
    let moof = w.begin(b"moof");
    let mfhd = w.full(b"mfhd", 0, 0);
    w.u32(seq);
    w.end(mfhd);
    let mut data_offset_slots = Vec::with_capacity(frags.len());
    for f in frags {
        let traf = w.begin(b"traf");
        // default-base-is-moof (0x020000); audio: default-sample-flags (0x20).
        let tfhd_flags = 0x02_0000 | if f.video { 0 } else { 0x20 };
        let tfhd = w.full(b"tfhd", 0, tfhd_flags);
        w.u32(f.track_id);
        if !f.video {
            w.u32(FLAG_SYNC);
        }
        w.end(tfhd);
        let tfdt = w.full(b"tfdt", 1, 0);
        w.u64(f.base_dts);
        w.end(tfdt);
        let trun_flags = 0x1 | 0x100 | 0x200 | if f.video { 0x400 | 0x800 } else { 0 };
        // Version 1: signed composition offsets (only matters if one is negative).
        let trun = w.full(b"trun", 1, trun_flags);
        w.u32(f.samples.len() as u32);
        data_offset_slots.push(w.0.len());
        w.i32(0);
        for s in &f.samples {
            w.u32(s.duration);
            w.u32(s.data.len());
            if f.video {
                w.u32(if s.sync { FLAG_SYNC } else { FLAG_NONSYNC });
                w.i32(s.cts);
            }
        }
        w.end(trun);
        w.end(traf);
    }
    w.end(moof);
    let moof_len = w.0.len() - moof_at;
    let payload_len: u64 = frags
        .iter()
        .flat_map(|f| f.samples.iter())
        .map(|s| s.data.len() as u64)
        .sum();
    let large = payload_len + 8 > u32::MAX as u64;
    let mdat_header = if large { 16 } else { 8 };
    let mut cursor = (moof_len + mdat_header) as u64;
    for (slot, f) in data_offset_slots.iter().zip(frags) {
        w.0[*slot..*slot + 4].copy_from_slice(&(cursor as i32).to_be_bytes());
        cursor += f.samples.iter().map(|s| s.data.len() as u64).sum::<u64>();
    }
    if large {
        w.u32(1);
        w.bytes(b"mdat");
        w.u64(payload_len + 16);
    } else {
        w.u32((payload_len + 8) as u32);
        w.bytes(b"mdat");
    }
    let mut body = SegmentBody::new();
    body.push(Part::Bytes(Bytes::from(w.0)));
    for f in frags {
        for s in &f.samples {
            match &s.data {
                SampleData::File { offset, len } => body.push(Part::File {
                    offset: *offset,
                    len: *len as u64,
                }),
                SampleData::Bytes(b) => body.push(Part::Bytes(b.clone())),
                SampleData::Prefixed { prefix, offset, len } => {
                    body.push(Part::Bytes(prefix.clone()));
                    body.push(Part::File {
                        offset: *offset,
                        len: *len as u64,
                    });
                }
            }
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use mp4_atom::{Any, Decode};

    fn aac_spec() -> InitSpec {
        InitSpec {
            track_id: 1,
            timescale: 48000,
            language: 0x15C7,
            entry: EntrySpec::Audio {
                fourcc: *b"mp4a",
                channels: 2,
                sample_rate: 48000,
                config: AudioConfig::Esds {
                    object_type: 0x40,
                    dsi: vec![0x11, 0x90],
                    avg_bitrate: 128_000,
                },
            },
        }
    }

    #[test]
    fn init_parses_with_mp4_atom() {
        let init = init_segment(&[aac_spec()]);
        let mut cur = std::io::Cursor::new(init.as_slice());
        let ftyp = Any::decode(&mut cur).unwrap();
        assert!(matches!(ftyp, Any::Ftyp(_)));
        let moov = Any::decode(&mut cur).unwrap();
        let Any::Moov(moov) = moov else {
            panic!("expected moov")
        };
        assert_eq!(moov.trak.len(), 1);
        assert_eq!(moov.trak[0].mdia.mdhd.timescale, 48000);
        assert_eq!(moov.trak[0].mdia.mdhd.language, "eng");
    }

    /// PB-9 / anamorphic: the init carries the source's rotation matrix in
    /// `tkhd` and a `pasp` box, and `tkhd` gives the display width.
    #[test]
    fn video_init_carries_matrix_and_pasp() {
        let rot90 = [0, 0x1_0000, 0, -0x1_0000, 0, 0, 1080 << 16, 0, 0x4000_0000];
        let spec = InitSpec {
            track_id: 1,
            timescale: 24000,
            language: 0x55C4,
            entry: EntrySpec::Video {
                fourcc: *b"avc1",
                config_box: *b"avcC",
                config: vec![1, 0x64, 0, 0x28, 0xFF, 0xE0, 0],
                width: 720,
                height: 576,
                color: None,
                mastering: None,
                cll: None,
                dovi: None,
                pasp: Some((64, 45)),
                matrix: Some(rot90),
            },
        };
        let init = init_segment(&[spec]);
        let mut cur = std::io::Cursor::new(init.as_slice());
        let _ftyp = Any::decode(&mut cur).unwrap();
        let Any::Moov(moov) = Any::decode(&mut cur).unwrap() else {
            panic!("moov")
        };
        let tkhd = &moov.trak[0].tkhd;
        let m = &tkhd.matrix;
        assert_eq!([m.a, m.b, m.c, m.d, m.x], [0, 0x1_0000, -0x1_0000, 0, 1080 << 16]);
        // 720 × 64/45 = 1024 display pixels wide.
        assert_eq!(tkhd.width.integer(), 1024);
        assert_eq!(tkhd.height.integer(), 576);
        let mp4_atom::Codec::Avc1(avc1) = &moov.trak[0].mdia.minf.stbl.stsd.codecs[0] else {
            panic!("avc1")
        };
        let pasp = avc1.pasp.as_ref().expect("pasp");
        assert_eq!((pasp.h_spacing, pasp.v_spacing), (64, 45));
    }

    #[test]
    fn media_segment_offsets() {
        let frag = Fragment {
            track_id: 1,
            base_dts: 4004,
            samples: vec![
                Sample {
                    duration: 1024,
                    cts: 0,
                    sync: true,
                    data: SampleData::Bytes(Bytes::from_static(b"abc")),
                },
                Sample {
                    duration: 1024,
                    cts: 0,
                    sync: true,
                    data: SampleData::Bytes(Bytes::from_static(b"de")),
                },
            ],
            video: false,
        };
        let body = media_segment(7, &[frag]);
        let bytes = body.read_in_memory().unwrap();
        let mut cur = std::io::Cursor::new(bytes.as_slice());
        let _styp = Any::decode(&mut cur).unwrap();
        let Any::Moof(moof) = Any::decode(&mut cur).unwrap() else {
            panic!("moof")
        };
        assert_eq!(moof.mfhd.sequence_number, 7);
        let traf = &moof.traf[0];
        assert_eq!(traf.tfdt.as_ref().unwrap().base_media_decode_time, 4004);
        let trun = &traf.trun[0];
        // default-base-is-moof: the offset is relative to the moof, after styp.
        let styp_len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let start = styp_len + trun.data_offset.unwrap() as usize;
        assert_eq!(&bytes[start..start + 5], b"abcde");
    }
}
