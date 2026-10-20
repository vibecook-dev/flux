//! Transport stream packets (ISO/IEC 13818-1 §2.4.3): 188 bytes starting
//! with the sync byte 0x47, or 192 in Blu-ray BDAV M2TS, where a 4-byte
//! arrival timestamp precedes each packet.

use std::io;

use crate::source::Source;

/// Bytes of one transport packet (without the M2TS prefix).
pub(crate) const TS_PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// Packet framing of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Framing {
    /// Stride between packets: 188 or 192.
    pub size: usize,
    /// Prefix before the sync byte: 0 or 4.
    pub lead: usize,
}

impl Framing {
    pub(crate) const TS: Framing = Framing { size: 188, lead: 0 };
    pub(crate) const M2TS: Framing = Framing { size: 192, lead: 4 };

    pub(crate) fn of(packet_size: u16) -> Framing {
        if packet_size == 192 {
            Framing::M2TS
        } else {
            Framing::TS
        }
    }
}

/// The framing and the offset of the first packet (prefix included), if
/// `head` holds transport packets: at least three, and up to eight
/// consecutive sync bytes at the stride.
pub(crate) fn detect(head: &[u8]) -> Option<(Framing, usize)> {
    [Framing::TS, Framing::M2TS]
        .into_iter()
        .find_map(|f| sync_offset(head, f).map(|o| (f, o)))
}

fn sync_offset(b: &[u8], f: Framing) -> Option<usize> {
    for start in 0..f.size {
        let packets = b.len().saturating_sub(start + f.lead) / f.size;
        if packets < 3 {
            return None;
        }
        let need = packets.min(8);
        if (0..need).all(|k| b[start + f.lead + k * f.size] == SYNC) {
            return Some(start);
        }
    }
    None
}

/// One transport packet.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Packet<'a> {
    /// File offset of the packet (its M2TS prefix included).
    pub pos: u64,
    pub pid: u16,
    /// payload_unit_start_indicator: a PES or section starts here.
    pub pusi: bool,
    /// transport_error_indicator: the packet is known to be damaged.
    pub tei: bool,
    pub scrambled: bool,
    pub cc: u8,
    pub has_payload: bool,
    /// The 188 bytes of the packet (without an M2TS prefix).
    pub raw: &'a [u8],
    /// Adaptation field discontinuity_indicator.
    pub discontinuity: bool,
    /// Adaptation field random_access_indicator.
    pub random_access: bool,
    pub payload: &'a [u8],
}

/// Parses the 188 bytes of a packet that starts with the sync byte.
pub(crate) fn parse(p: &[u8], pos: u64) -> Option<Packet<'_>> {
    if p.len() < TS_PACKET || p[0] != SYNC {
        return None;
    }
    let afc = (p[3] >> 4) & 3;
    if afc == 0 {
        return None; // reserved: discard
    }
    let mut discontinuity = false;
    let mut random_access = false;
    let mut start = 4usize;
    if afc & 2 != 0 {
        let al = p[4] as usize;
        if al > 183 {
            return None;
        }
        if al > 0 {
            discontinuity = p[5] & 0x80 != 0;
            random_access = p[5] & 0x40 != 0;
        }
        start = 5 + al;
    }
    let has_payload = afc & 1 != 0;
    Some(Packet {
        pos,
        pid: u16::from_be_bytes([p[1] & 0x1F, p[2]]),
        pusi: p[1] & 0x40 != 0,
        tei: p[1] & 0x80 != 0,
        scrambled: p[3] >> 6 != 0,
        cc: p[3] & 0x0F,
        has_payload,
        raw: &p[..TS_PACKET],
        discontinuity,
        random_access,
        payload: if has_payload {
            &p[start.min(TS_PACKET)..TS_PACKET]
        } else {
            &[]
        },
    })
}

/// Walks the packets of `buf`, whose first byte is at file offset `base`
/// and should start a packet. Lost sync (garbage between packets) is
/// skipped: the walk resumes where two sync bytes are a stride apart.
/// Returns the bytes consumed, i.e. up to the first incomplete packet, for
/// the caller to carry into its next read.
pub(crate) fn walk(
    buf: &[u8],
    base: u64,
    f: Framing,
    mut each: impl FnMut(&Packet<'_>) -> bool,
) -> (usize, bool) {
    let mut p = 0usize;
    while p + f.lead + TS_PACKET <= buf.len() {
        if buf[p + f.lead] == SYNC {
            if let Some(pk) = parse(&buf[p + f.lead..p + f.lead + TS_PACKET], base + p as u64) {
                if !each(&pk) {
                    return (p + f.size, false);
                }
            }
            p += f.size;
            continue;
        }
        // Resynchronise: the next sync byte confirmed one stride later.
        let mut q = p + 1;
        let mut found = None;
        while q + f.lead < buf.len() {
            if buf[q + f.lead] == SYNC {
                let next = q + f.lead + f.size;
                if next >= buf.len() {
                    // Can't confirm yet: let the caller re-read from here.
                    return (q, true);
                }
                if buf[next] == SYNC {
                    found = Some(q);
                    break;
                }
            }
            q += 1;
        }
        match found {
            Some(q) => p = q,
            None => return (buf.len().saturating_sub(f.lead + f.size).max(p + 1), true),
        }
    }
    (p, true)
}

/// Calls `each` for every packet that starts in `[from, to)` of the file,
/// reading `chunk` bytes at a time. `each` returns false to stop early.
pub(crate) fn scan(
    src: &Source,
    from: u64,
    to: u64,
    f: Framing,
    chunk: usize,
    mut each: impl FnMut(&Packet<'_>) -> bool,
) -> io::Result<()> {
    let end = to.min(src.len());
    let chunk = chunk.max(f.size * 64) / f.size * f.size;
    let mut pos = from;
    while pos < end {
        // Read whole packets past `to` so the last one is complete.
        let want = (chunk as u64).min(end - pos + f.size as u64) as usize;
        let buf = src.read_at(pos, want)?;
        if buf.len() < f.lead + TS_PACKET {
            break;
        }
        let mut stop = false;
        let (used, more) = walk(&buf, pos, f, |pk| {
            if pk.pos >= end {
                stop = true;
                return false;
            }
            each(pk)
        });
        if stop || !more || used == 0 {
            break;
        }
        pos += used as u64;
    }
    Ok(())
}

/// Continuity of one PID (§2.4.3.3): the 4-bit counter increments with
/// every packet that carries payload; a packet may be repeated once.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Continuity {
    last: Option<u8>,
}

/// What a packet means for its PID's continuity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cc {
    Ok,
    /// A repeated packet: its payload was already used.
    Duplicate,
    /// Packets were lost before this one.
    Lost,
}

impl Continuity {
    pub(crate) fn check(&mut self, pk: &Packet<'_>) -> Cc {
        if !pk.has_payload {
            return Cc::Ok;
        }
        let r = match self.last {
            _ if pk.discontinuity => Cc::Ok,
            None => Cc::Ok,
            Some(l) if l == pk.cc => Cc::Duplicate,
            Some(l) if (l + 1) & 0x0F == pk.cc => Cc::Ok,
            Some(_) => Cc::Lost,
        };
        self.last = Some(pk.cc);
        r
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A packet with `payload` (padded with an adaptation field).
    pub(crate) fn packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 184);
        let mut p = vec![SYNC, ((pusi as u8) << 6) | (pid >> 8) as u8, pid as u8];
        let stuffing = 184 - payload.len();
        if stuffing == 0 {
            p.push(0x10 | (cc & 15));
        } else {
            p.push(0x30 | (cc & 15));
            p.push((stuffing - 1) as u8);
            if stuffing > 1 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xFF, stuffing - 2));
            }
        }
        p.extend_from_slice(payload);
        assert_eq!(p.len(), 188);
        p
    }

    #[test]
    fn detects_ts_and_m2ts() {
        let mut ts = vec![0u8; 3];
        for i in 0..6 {
            ts.extend(packet(0x100, false, i, &[1, 2, 3]));
        }
        assert_eq!(detect(&ts), Some((Framing::TS, 3)));
        let mut m2ts = Vec::new();
        for i in 0..6 {
            m2ts.extend([0, 0, 0, i]);
            m2ts.extend(packet(0x100, false, i, &[]));
        }
        assert_eq!(detect(&m2ts), Some((Framing::M2TS, 0)));
        assert_eq!(detect(&[0x47; 100]), None);
    }

    #[test]
    fn walk_resyncs_after_garbage() {
        let mut b = Vec::new();
        b.extend(packet(0x100, true, 0, &[9; 10]));
        b.extend([1, 0x47, 2, 3, 4]); // garbage with a false sync byte
        b.extend(packet(0x101, true, 0, &[8; 10]));
        b.extend(packet(0x102, true, 0, &[7; 10]));
        let mut pids = Vec::new();
        let (used, _) = walk(&b, 1000, Framing::TS, |p| {
            pids.push((p.pid, p.pos));
            true
        });
        assert_eq!(
            pids,
            vec![(0x100, 1000), (0x101, 1000 + 193), (0x102, 1000 + 381)]
        );
        assert_eq!(used, b.len());
    }

    #[test]
    fn continuity_counts_duplicates_and_losses() {
        let mut c = Continuity::default();
        let raw = |cc| packet(0x100, false, cc, &[0; 4]);
        let check = |c: &mut Continuity, cc: u8| {
            let b = raw(cc);
            c.check(&parse(&b, 0).unwrap())
        };
        assert_eq!(check(&mut c, 14), Cc::Ok);
        assert_eq!(check(&mut c, 15), Cc::Ok);
        assert_eq!(check(&mut c, 15), Cc::Duplicate);
        assert_eq!(check(&mut c, 0), Cc::Ok);
        assert_eq!(check(&mut c, 3), Cc::Lost);
        assert_eq!(check(&mut c, 4), Cc::Ok);
    }
}
