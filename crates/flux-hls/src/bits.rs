//! Bit-level reading for codec headers (SPS, AC-3/E-AC-3/DTS sync frames, …).

/// MSB-first bit reader over a byte slice. Reads past the end return `None`.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    pub(crate) fn bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.pos / 8)?;
        let v = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(v == 1)
    }

    /// Reads `n` ≤ 64 bits as an unsigned integer.
    pub(crate) fn bits(&mut self, n: u32) -> Option<u64> {
        debug_assert!(n <= 64);
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        Some(v)
    }

    pub(crate) fn u8(&mut self, n: u32) -> Option<u8> {
        self.bits(n).map(|v| v as u8)
    }

    pub(crate) fn u16(&mut self, n: u32) -> Option<u16> {
        self.bits(n).map(|v| v as u16)
    }

    pub(crate) fn u32(&mut self, n: u32) -> Option<u32> {
        self.bits(n).map(|v| v as u32)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// Unsigned Exp-Golomb (H.264/HEVC `ue(v)`).
    pub(crate) fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)? as u32;
        Some(((1u64 << zeros) - 1) as u32 + rest)
    }

    /// Signed Exp-Golomb (`se(v)`).
    pub(crate) fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 {
            ((k + 1) / 2) as i32
        } else {
            -(k / 2) as i32
        })
    }
}

/// Removes H.264/HEVC emulation-prevention bytes (`00 00 03` → `00 00`) from a
/// NAL unit payload so it can be parsed as an RBSP.
pub(crate) fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// Iterates the NAL units of a length-prefixed (AVCC/HVCC) sample.
pub(crate) fn length_prefixed_nals(
    sample: &[u8],
    length_size: usize,
) -> impl Iterator<Item = (usize, &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if length_size == 0 || pos + length_size > sample.len() {
            return None;
        }
        let mut n = 0usize;
        for i in 0..length_size {
            n = (n << 8) | sample[pos + i] as usize;
        }
        let start = pos + length_size;
        let end = start.checked_add(n)?;
        if end > sample.len() {
            return None;
        }
        pos = end;
        Some((start - length_size, &sample[start..end]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // 1 -> 0, 010 -> 1, 011 -> 2, 00100 -> 3
        let data = [0b1010_0110, 0b0100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.ue(), Some(1));
        assert_eq!(r.ue(), Some(2));
        assert_eq!(r.ue(), Some(3));
    }

    #[test]
    fn signed_exp_golomb() {
        // ue 1 -> +1, ue 2 -> -1
        let data = [0b0100_1100];
        let mut r = BitReader::new(&data);
        assert_eq!(r.se(), Some(1));
        assert_eq!(r.se(), Some(-1));
    }

    #[test]
    fn rbsp_unescape() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1, 0, 0, 3, 0]), vec![0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn nal_iteration() {
        let s = [0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 1, 0x06];
        let nals: Vec<_> = length_prefixed_nals(&s, 4).map(|(_, n)| n.to_vec()).collect();
        assert_eq!(nals, vec![vec![0x65, 0xAA], vec![0x06]]);
    }
}
