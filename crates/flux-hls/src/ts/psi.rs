//! Program-specific information: PAT and PMT sections (ISO/IEC 13818-1
//! §2.4.4), and the stream classification flux-probe uses, so both name the
//! same tracks the same way (track order, kinds and codecs must agree for
//! `plan_from_probe` to equal the engine's plan).

use flux_core::{Codec, TrackKind};

use super::packet::Packet;

/// A PSI section being assembled from packets of one PID.
#[derive(Debug, Default)]
pub(crate) struct Section {
    buf: Vec<u8>,
}

impl Section {
    /// Feeds a packet payload; returns each complete section (without CRC check).
    pub(crate) fn push(&mut self, pk: &Packet<'_>) -> Option<Vec<u8>> {
        let payload = pk.payload;
        if pk.pusi {
            let ptr = *payload.first()? as usize;
            self.buf = payload.get(1 + ptr..)?.to_vec();
        } else if !self.buf.is_empty() {
            self.buf.extend_from_slice(payload);
        } else {
            return None;
        }
        if self.buf.first() == Some(&0xFF) {
            self.buf.clear();
            return None;
        }
        let len = (u16::from_be_bytes([*self.buf.get(1)?, *self.buf.get(2)?]) & 0x0FFF) as usize;
        if self.buf.len() >= 3 + len {
            let mut s = std::mem::take(&mut self.buf);
            s.truncate(3 + len);
            return Some(s);
        }
        None
    }
}

/// PMT PIDs listed by a PAT section.
pub(crate) fn parse_pat(s: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    if s.first() != Some(&0) || s.len() < 12 {
        return out;
    }
    let end = s.len() - 4;
    let mut i = 8;
    while i + 4 <= end {
        let program = u16::from_be_bytes([s[i], s[i + 1]]);
        let pid = u16::from_be_bytes([s[i + 2] & 0x1F, s[i + 3]]);
        if program != 0 {
            out.push(pid);
        }
        i += 4;
    }
    out
}

/// One elementary stream of a PMT.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Es {
    pub pid: u16,
    pub stream_type: u8,
    pub program: u16,
    pub registration: Option<[u8; 4]>,
    pub language: Option<String>,
    pub hearing_impaired: bool,
    pub descriptor_codec: Option<(TrackKind, Codec)>,
}

fn descriptors(mut d: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    while d.len() >= 2 {
        let (tag, len) = (d[0], d[1] as usize);
        let Some(body) = d.get(2..2 + len) else { break };
        out.push((tag, body));
        d = &d[2 + len..];
    }
    out
}

/// A PMT section: (program number, HDMV registration, PCR PID, streams).
pub(crate) fn parse_pmt(s: &[u8]) -> Option<(u16, bool, u16, Vec<Es>)> {
    if s.first() != Some(&2) || s.len() < 16 {
        return None;
    }
    let program = u16::from_be_bytes([s[3], s[4]]);
    let pcr_pid = u16::from_be_bytes([s[8] & 0x1F, s[9]]);
    let end = s.len() - 4;
    let pil = (u16::from_be_bytes([s[10] & 0x0F, s[11]])) as usize;
    let prog_desc = s.get(12..12 + pil)?;
    let hdmv = descriptors(prog_desc)
        .iter()
        .any(|(tag, body)| *tag == 0x05 && body.get(..4) == Some(b"HDMV"));
    let mut streams = Vec::new();
    let mut i = 12 + pil;
    while i + 5 <= end {
        let stream_type = s[i];
        let pid = u16::from_be_bytes([s[i + 1] & 0x1F, s[i + 2]]);
        let eil = (u16::from_be_bytes([s[i + 3] & 0x0F, s[i + 4]])) as usize;
        let Some(desc) = s.get(i + 5..i + 5 + eil) else {
            break;
        };
        i += 5 + eil;
        let mut es = Es {
            pid,
            stream_type,
            program,
            ..Es::default()
        };
        for (tag, body) in descriptors(desc) {
            match tag {
                0x05 => es.registration = body.get(..4).and_then(|b| b.try_into().ok()),
                0x0A => {
                    if let Some(l) = body.get(..3) {
                        es.language =
                            Some(String::from_utf8_lossy(l).trim().to_string()).filter(|l| !l.is_empty());
                        es.hearing_impaired = body.get(3) == Some(&2);
                    }
                }
                0x59 => {
                    es.descriptor_codec = Some((TrackKind::Subtitle, Codec::DvbSub));
                    if let Some(l) = body.get(..3) {
                        es.language = Some(String::from_utf8_lossy(l).trim().to_string());
                        es.hearing_impaired = body.get(3).is_some_and(|t| (0x20..=0x24).contains(t));
                    }
                }
                0x56 => es.descriptor_codec = Some((TrackKind::Subtitle, Codec::Other)),
                0x6A => es.descriptor_codec = Some((TrackKind::Audio, Codec::Ac3)),
                0x7A => es.descriptor_codec = Some((TrackKind::Audio, Codec::Eac3)),
                0x7B => es.descriptor_codec = Some((TrackKind::Audio, Codec::Dts)),
                0x7C => es.descriptor_codec = Some((TrackKind::Audio, Codec::Aac)),
                _ => {}
            }
        }
        streams.push(es);
    }
    Some((program, hdmv, pcr_pid, streams))
}

/// Stream type (+ registration / descriptors) → kind and codec, as
/// flux-probe classifies it (`flux_probe::ts`).
pub(crate) fn classify(es: &Es, hdmv: bool) -> (TrackKind, Codec) {
    use Codec::*;
    use TrackKind::*;
    let reg = es.registration.as_ref().map(|r| &r[..]);
    match es.stream_type {
        0x01 => (Video, Other),
        0x02 => (Video, Mpeg2Video),
        0x03 | 0x04 => (Audio, Mp2),
        0x0F | 0x11 => (Audio, Aac),
        0x10 => (Video, Mpeg4Part2),
        0x1B => (Video, H264),
        0x24 => (Video, Hevc),
        0xEA => (Video, Vc1),
        0x80 if hdmv => (Audio, Pcm),
        0x81 => (Audio, Ac3),
        0x82 => (Audio, Dts),
        0x83 if hdmv => (Audio, TrueHd),
        0x84 | 0xA1 if hdmv => (Audio, Eac3),
        0x85 | 0x86 | 0xA2 if hdmv => (Audio, Dts),
        0x87 => (Audio, Eac3),
        0x90 => (Subtitle, Pgs),
        0x92 if hdmv => (Subtitle, Other),
        0x06 => match (es.descriptor_codec, reg) {
            (Some(kc), _) => kc,
            (None, Some(b"AC-3")) => (Audio, Ac3),
            (None, Some(b"EAC3")) => (Audio, Eac3),
            (None, Some(b"DTS1" | b"DTS2" | b"DTS3")) => (Audio, Dts),
            (None, Some(b"Opus")) => (Audio, Opus),
            (None, Some(b"BSSD")) => (Audio, Pcm),
            (None, Some(b"HEVC")) => (Video, Hevc),
            _ => (Data, Other), // refined by sniffing the payload
        },
        _ => (Data, Other),
    }
}

/// Streams worth waiting for in the head (flux-probe's `could_be_av`).
pub(crate) fn could_be_av(es: &Es) -> bool {
    matches!(
        es.stream_type,
        0x01..=0x04 | 0x06 | 0x0F | 0x10 | 0x11 | 0x1B | 0x24 | 0x80..=0x87 | 0xA1 | 0xA2 | 0xEA
    ) && !matches!(es.descriptor_codec, Some((TrackKind::Subtitle, _)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdmv_pmt_with_private_aac() {
        // PMT of features/h264_aac.m2ts (ffmpeg, m2ts mode).
        let pmt = [
            0x02, 0xB0, 0x23, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xF0, 0x11, 0xF0, 0x0C, 0x05, 0x04, 0x48, 0x44,
            0x4D, 0x56, 0x88, 0x04, 0x0F, 0xFF, 0xFC, 0xFC, 0x1B, 0xF0, 0x11, 0xF0, 0x00, 0x06, 0xF1, 0x00,
            0xF0, 0x00, 0xAE, 0x0C, 0x0C, 0x26,
        ];
        let (program, hdmv, pcr, es) = parse_pmt(&pmt).unwrap();
        assert_eq!((program, hdmv, pcr), (1, true, 0x1011));
        assert_eq!(es.len(), 2);
        assert_eq!(classify(&es[0], hdmv), (TrackKind::Video, Codec::H264));
        // Private data without a descriptor: refined by sniffing (ADTS).
        assert_eq!(classify(&es[1], hdmv), (TrackKind::Data, Codec::Other));
        assert!(could_be_av(&es[1]));
    }
}
