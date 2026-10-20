//! Bounds-checked byte and bit readers.
//!
//! Every accessor returns `Option`, so malformed or truncated input can never
//! panic: a short read is just `None`, which callers turn into a warning or a
//! `Malformed` error.

/// Big/little-endian cursor over a byte slice.
#[derive(Debug, Clone)]
pub(crate) struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Rd<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Rd { b, pos: 0 }
    }
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }
    pub(crate) fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.pos)
    }
    pub(crate) fn rest(&self) -> &'a [u8] {
        self.b.get(self.pos..).unwrap_or(&[])
    }
    pub(crate) fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }
    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.b.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    pub(crate) fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    pub(crate) fn be16(&mut self) -> Option<u16> {
        self.arr().map(u16::from_be_bytes)
    }
    pub(crate) fn be24(&mut self) -> Option<u32> {
        let [a, b, c] = self.arr::<3>()?;
        Some(u32::from_be_bytes([0, a, b, c]))
    }
    pub(crate) fn be32(&mut self) -> Option<u32> {
        self.arr().map(u32::from_be_bytes)
    }
    pub(crate) fn be64(&mut self) -> Option<u64> {
        self.arr().map(u64::from_be_bytes)
    }
    pub(crate) fn le16(&mut self) -> Option<u16> {
        self.arr().map(u16::from_le_bytes)
    }
    pub(crate) fn le32(&mut self) -> Option<u32> {
        self.arr().map(u32::from_le_bytes)
    }
    pub(crate) fn fourcc(&mut self) -> Option<[u8; 4]> {
        self.arr()
    }
    /// AV1 / ISO-BMFF `leb128()`: little-endian base-128, at most 8 bytes.
    pub(crate) fn leb128(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for i in 0..8 {
            let b = self.u8()?;
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
}

/// MSB-first bit reader with Exp-Golomb support (H.264/HEVC/AV1 syntax).
#[derive(Debug, Clone)]
pub(crate) struct Bits<'a> {
    b: &'a [u8],
    bit: usize,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Bits { b, bit: 0 }
    }
    pub(crate) fn bits_left(&self) -> usize {
        (self.b.len() * 8).saturating_sub(self.bit)
    }
    pub(crate) fn skip(&mut self, n: usize) -> Option<()> {
        if n > self.bits_left() {
            return None;
        }
        self.bit += n;
        Some(())
    }
    pub(crate) fn flag(&mut self) -> Option<bool> {
        let byte = *self.b.get(self.bit / 8)?;
        let v = (byte >> (7 - (self.bit % 8))) & 1;
        self.bit += 1;
        Some(v == 1)
    }
    /// Read `n` ≤ 32 bits.
    pub(crate) fn u(&mut self, n: u32) -> Option<u32> {
        if n > 32 {
            return None;
        }
        self.u64(n).map(|v| v as u32)
    }
    /// Read `n` ≤ 64 bits.
    pub(crate) fn u64(&mut self, n: u32) -> Option<u64> {
        if n > 64 || n as usize > self.bits_left() {
            return None;
        }
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.flag()?);
        }
        Some(v)
    }
    pub(crate) fn u8(&mut self, n: u32) -> Option<u8> {
        if n > 8 {
            return None;
        }
        self.u(n).map(|v| v as u8)
    }
    /// Unsigned Exp-Golomb `ue(v)`; values above 2^32-2 are rejected.
    pub(crate) fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.u(zeros)?;
        let v = (1u64 << zeros) - 1 + u64::from(rest);
        u32::try_from(v).ok()
    }
    /// Signed Exp-Golomb `se(v)`.
    pub(crate) fn se(&mut self) -> Option<i32> {
        let k = i64::from(self.ue()?);
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        i32::try_from(v).ok()
    }
    /// AV1 `uvlc()`.
    pub(crate) fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while !self.flag()? {
            zeros += 1;
            if zeros >= 32 {
                return Some(u32::MAX);
            }
        }
        let v = self.u(zeros)?;
        Some(((1u64 << zeros) - 1 + u64::from(v)).min(u64::from(u32::MAX)) as u32)
    }
}

/// Strip H.264/HEVC emulation-prevention bytes (`00 00 03` → `00 00`) to get
/// the RBSP that the bit-level syntax is defined over.
pub(crate) fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0usize;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Split an Annex B byte stream (`00 00 01` / `00 00 00 01` start codes) into
/// NAL units. Trailing zero bytes of each unit are trimmed.
pub(crate) fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if let Some(nal) = data.get(s..end) {
            if !nal.is_empty() {
                out.push(nal);
            }
        }
    }
    out
}

/// Decode a UTF-8 string, dropping trailing NULs and replacing invalid bytes.
pub(crate) fn string(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// FourCC as printable text (`avc1`, `0x00000000` when not printable).
pub(crate) fn fourcc_str(f: [u8; 4]) -> String {
    if f.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        String::from_utf8_lossy(&f).to_string()
    } else {
        format!("0x{:08x}", u32::from_be_bytes(f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // 1 → 0, 010 → 1, 011 → 2, 00100 → 3
        let data = [0b1010_0110, 0b0100_0000];
        let mut b = Bits::new(&data);
        assert_eq!(b.ue(), Some(0));
        assert_eq!(b.ue(), Some(1));
        assert_eq!(b.ue(), Some(2));
        assert_eq!(b.ue(), Some(3));
        assert_eq!(Bits::new(&[0, 0, 0, 0, 0]).ue(), None);
    }

    #[test]
    fn rbsp_strips_emulation_prevention() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
    }

    #[test]
    fn annexb_split() {
        let d = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0];
        let n = annexb_nals(&d);
        assert_eq!(n, vec![&[0x67, 1, 2][..], &[0x68, 3][..]]);
    }

    #[test]
    fn short_reads_are_none() {
        let mut r = Rd::new(&[1, 2]);
        assert_eq!(r.be32(), None);
        assert_eq!(r.be16(), Some(0x0102));
        assert_eq!(r.u8(), None);
    }
}
