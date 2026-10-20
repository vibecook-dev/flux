//! PES packets (ISO/IEC 13818-1 §2.4.3.6): header with PTS/DTS, payload
//! reassembled from the transport packets of one PID.

use super::packet::{Cc, Continuity, Packet};

/// One wrap of the 33-bit 90 kHz clock.
pub(crate) const WRAP: i64 = 1 << 33;

/// A parsed PES header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PesHeader {
    pub stream_id: u8,
    /// 33-bit presentation / decoding timestamps (90 kHz).
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    /// Bytes before the payload.
    pub header_len: usize,
    /// PES_packet_length: bytes after the length field (0 = unbounded).
    pub packet_len: usize,
}

fn ts33(b: &[u8]) -> u64 {
    (u64::from(b[0] >> 1) & 7) << 30
        | u64::from(b[1]) << 22
        | u64::from(b[2] >> 1) << 15
        | u64::from(b[3]) << 7
        | u64::from(b[4] >> 1)
}

/// Parses a PES header; `None` if `b` is not a PES start or is too short to
/// hold the whole header yet.
pub(crate) fn parse_header(b: &[u8]) -> Option<PesHeader> {
    if b.len() < 6 || b[..3] != [0, 0, 1] {
        return None;
    }
    let stream_id = b[3];
    let packet_len = u16::from_be_bytes([b[4], b[5]]) as usize;
    // Streams without the optional header (program stream map, padding,
    // private_stream_2, ECM/EMM, DSM-CC, H.222.1 type E, directory).
    if matches!(stream_id, 0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF) {
        return Some(PesHeader {
            stream_id,
            pts: None,
            dts: None,
            header_len: 6,
            packet_len,
        });
    }
    if b.len() < 9 || b[6] & 0xC0 != 0x80 {
        return None;
    }
    let flags = b[7];
    let header_len = 9 + b[8] as usize;
    if b.len() < header_len {
        return None;
    }
    let pts = (flags & 0x80 != 0 && header_len >= 14).then(|| ts33(&b[9..14]));
    let dts = (flags & 0xC0 == 0xC0 && header_len >= 19).then(|| ts33(&b[14..19]));
    Some(PesHeader {
        stream_id,
        pts,
        dts,
        header_len,
        packet_len,
    })
}

/// Unwraps a 33-bit timestamp to the value nearest `near` (same units).
pub(crate) fn unwrap_near(raw: u64, near: i64) -> i64 {
    let raw = raw as i64 & (WRAP - 1);
    let d = (raw - near).rem_euclid(WRAP);
    let d = if d >= WRAP / 2 { d - WRAP } else { d };
    near + d
}

/// A reassembled PES packet.
#[derive(Debug, Clone)]
pub(crate) struct Pes {
    /// File offset of the transport packet that started it.
    pub pos: u64,
    pub header: PesHeader,
    pub payload: Vec<u8>,
    /// Packets were lost or damaged inside it.
    pub damaged: bool,
    /// The whole PES was seen (its length was reached or the next began).
    pub complete: bool,
}

struct Pending {
    pos: u64,
    /// Bytes until the header is complete, then the payload.
    buf: Vec<u8>,
    header: Option<PesHeader>,
    damaged: bool,
}

/// Reassembles the PES packets of one PID, checking continuity.
#[derive(Default)]
pub(crate) struct Assembler {
    cur: Option<Pending>,
    cc: Continuity,
    /// Continuity errors so far.
    pub lost: u32,
}

impl Pending {
    fn finish(self, complete: bool) -> Option<Pes> {
        let header = self.header?;
        let mut payload = self.buf;
        if header.packet_len > 0 {
            let want = (header.packet_len + 6).saturating_sub(header.header_len);
            payload.truncate(want);
        }
        Some(Pes {
            pos: self.pos,
            header,
            payload,
            damaged: self.damaged,
            complete,
        })
    }

    /// True once a bounded PES has all its bytes.
    fn full(&self) -> bool {
        match self.header {
            Some(h) if h.packet_len > 0 => self.buf.len() + h.header_len >= h.packet_len + 6,
            _ => false,
        }
    }
}

impl Assembler {
    /// Feeds one packet of this PID; pushes any PES it completes to `out`.
    pub(crate) fn push(&mut self, pk: &Packet<'_>, out: &mut Vec<Pes>) {
        let cc = self.cc.check(pk);
        if cc == Cc::Duplicate || !pk.has_payload {
            return;
        }
        let damaged = cc == Cc::Lost || pk.tei;
        if cc == Cc::Lost {
            self.lost += 1;
        }
        if pk.pusi {
            if let Some(mut p) = self.cur.take() {
                // Lost packets before this start belonged to the previous PES.
                p.damaged |= cc == Cc::Lost;
                if let Some(p) = p.finish(true) {
                    out.push(p);
                }
            }
            let mut p = Pending {
                pos: pk.pos,
                buf: pk.payload.to_vec(),
                header: None,
                damaged: pk.tei,
            };
            p.parse_header();
            self.cur = Some(p);
        } else if let Some(p) = self.cur.as_mut() {
            p.damaged |= damaged;
            p.buf.extend_from_slice(pk.payload);
            if p.header.is_none() {
                p.parse_header();
            }
        } else {
            return;
        }
        if self.cur.as_ref().is_some_and(|p| p.full()) {
            if let Some(p) = self.cur.take().and_then(|p| p.finish(true)) {
                out.push(p);
            }
        }
    }

    /// Ends the input: returns the PES in progress, `complete` only at the
    /// end of the stream (otherwise its tail may be missing).
    pub(crate) fn finish(&mut self, end_of_stream: bool) -> Option<Pes> {
        self.cur.take().and_then(|p| p.finish(end_of_stream))
    }
}

impl Pending {
    fn parse_header(&mut self) {
        match parse_header(&self.buf) {
            Some(h) => {
                self.buf.drain(..h.header_len.min(self.buf.len()));
                self.header = Some(h);
            }
            // Not a PES start at all, or a header longer than any packet
            // could hold: drop it.
            None if self.buf.len() >= 6 && self.buf[..3] != [0, 0, 1] => self.buf.clear(),
            None if self.buf.len() > 300 => self.buf.clear(),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::packet::{parse, tests::packet};
    use super::*;

    fn pes_bytes(pts: u64, dts: Option<u64>, payload: &[u8], bounded: bool) -> Vec<u8> {
        let enc = |marker: u8, t: u64| {
            [
                (marker << 4) | (((t >> 30) as u8 & 7) << 1) | 1,
                (t >> 22) as u8,
                (((t >> 15) as u8) << 1) | 1,
                (t >> 7) as u8,
                ((t as u8) << 1) | 1,
            ]
        };
        let mut h = vec![0, 0, 1, 0xE0, 0, 0, 0x80];
        match dts {
            Some(d) => {
                h.extend([0xC0, 10]);
                h.extend(enc(3, pts));
                h.extend(enc(1, d));
            }
            None => {
                h.extend([0x80, 5]);
                h.extend(enc(2, pts));
            }
        }
        if bounded {
            let n = h.len() - 6 + payload.len();
            h[4] = (n >> 8) as u8;
            h[5] = n as u8;
        }
        h.extend_from_slice(payload);
        h
    }

    #[test]
    fn header_timestamps() {
        let b = pes_bytes((1 << 32) | 5, Some(7), &[1, 2], false);
        let h = parse_header(&b).unwrap();
        assert_eq!((h.pts, h.dts, h.header_len), (Some((1 << 32) | 5), Some(7), 19));
        assert!(parse_header(&b[..12]).is_none());
    }

    #[test]
    fn unwraps_across_the_33_bit_wrap() {
        let near = WRAP - 90_000; // 1 s before the wrap
        assert_eq!(unwrap_near(90_000, near), WRAP + 90_000);
        assert_eq!(unwrap_near((WRAP - 180_000) as u64, near), WRAP - 180_000);
        assert_eq!(unwrap_near(5, -10), 5);
        assert_eq!(unwrap_near((WRAP - 10) as u64, 0), -10);
    }

    #[test]
    fn reassembles_unbounded_and_bounded_pes() {
        let payload: Vec<u8> = (0..400u32).map(|i| i as u8).collect();
        let v = pes_bytes(900, Some(0), &payload, false);
        let mut a = Assembler::default();
        let mut out = Vec::new();
        let (p1, p2, p3) = (&v[..184], &v[184..368], &v[368..]);
        for (i, (chunk, pusi)) in [(p1, true), (p2, false), (p3, false)].into_iter().enumerate() {
            let b = packet(0x100, pusi, i as u8, chunk);
            a.push(&parse(&b, i as u64 * 188).unwrap(), &mut out);
        }
        assert!(out.is_empty(), "unbounded: waits for the next start");
        let b = packet(0x100, true, 3, &pes_bytes(1800, None, &[9], true));
        a.push(&parse(&b, 564).unwrap(), &mut out);
        // The bounded one completes at once.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].payload, payload);
        assert_eq!(
            (out[0].header.pts, out[0].pos, out[0].damaged),
            (Some(900), 0, false)
        );
        assert_eq!((out[1].payload.clone(), out[1].header.pts), (vec![9], Some(1800)));
        assert!(a.finish(true).is_none());
    }
}
