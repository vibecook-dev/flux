//! Shared helpers for the integration tests.
#![allow(dead_code)]

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use flux_core::ProbeResult;
use flux_probe::{ContainerHint, ProbeError, Prober, RangeRead, Step};

/// The corpus feature clips, `$FLUX_TEST_MEDIA/features` (a path that
/// never exists when the variable is unset, so the tests skip).
pub fn features() -> PathBuf {
    flux_test_media::media_dir("features")
}

/// The probe fixtures, `$FLUX_TEST_MEDIA/probe-fixtures` (see [`features`]).
pub fn fixtures() -> PathBuf {
    flux_test_media::media_dir("probe-fixtures")
}

/// A sparse in-memory copy of a file: real head and tail bytes, zeros in
/// between (the probe never reads the middle of these files). Keeps the real
/// file size so tail reads (TS last PTS, MP4 moov-at-end) are exercised.
#[derive(Clone)]
pub struct MemFile {
    pub size: u64,
    pub head: Vec<u8>,
    pub tail_off: u64,
    pub tail: Vec<u8>,
}

impl MemFile {
    pub fn whole(d: &[u8]) -> Self {
        MemFile {
            size: d.len() as u64,
            head: d.to_vec(),
            tail_off: d.len() as u64,
            tail: Vec::new(),
        }
    }

    /// Head (`head` bytes) and tail (`tail` bytes) of a real file.
    pub fn load(p: &Path, head: u64, tail: u64) -> Option<Self> {
        let mut f = std::fs::File::open(p).ok()?;
        let size = f.metadata().ok()?.len();
        let mut h = Vec::new();
        f.by_ref().take(head).read_to_end(&mut h).ok()?;
        let tail_off = size.saturating_sub(tail).max(h.len() as u64);
        f.seek(SeekFrom::Start(tail_off)).ok()?;
        let mut t = Vec::new();
        f.read_to_end(&mut t).ok()?;
        Some(MemFile {
            size,
            head: h,
            tail_off,
            tail: t,
        })
    }

    pub fn read(&self, off: u64, len: u32) -> Vec<u8> {
        let end = (off + u64::from(len)).min(self.size);
        let mut out = vec![0u8; end.saturating_sub(off) as usize];
        for (i, b) in out.iter_mut().enumerate() {
            let at = off + i as u64;
            if (at as usize) < self.head.len() {
                *b = self.head[at as usize];
            } else if at >= self.tail_off {
                *b = self.tail[(at - self.tail_off) as usize];
            }
        }
        out
    }

    /// Truncate to `n` bytes.
    pub fn prefix(&self, n: u64) -> Self {
        let n = n.min(self.size);
        let head = self.head[..(n as usize).min(self.head.len())].to_vec();
        let tail = if n > self.tail_off {
            self.tail[..(n - self.tail_off) as usize].to_vec()
        } else {
            Vec::new()
        };
        MemFile {
            size: n,
            head,
            tail_off: self.tail_off.min(n),
            tail,
        }
    }
}

impl RangeRead for MemFile {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let b = MemFile::read(self, offset, buf.len() as u32);
        buf[..b.len()].copy_from_slice(&b);
        Ok(b.len())
    }
}

/// Drive the sans-IO header prober over any reader, feeding exactly what it asks.
pub fn probe_with(size: u64, read: impl Fn(u64, u32) -> Vec<u8>) -> Result<ProbeResult, ProbeError> {
    let mut p = Prober::new(size, ContainerHint::NONE);
    let mut step = p.start()?;
    for _ in 0..200 {
        match step {
            Step::Need { offset, len } => step = p.feed(offset, &read(offset, len))?,
            Step::Done(r) => return Ok(r),
        }
    }
    panic!("prober did not converge in 200 steps");
}

pub fn probe_mf(f: &MemFile) -> Result<ProbeResult, ProbeError> {
    probe_with(f.size, |o, l| f.read(o, l))
}

/// Small deterministic PRNG (xorshift64*), so failures are reproducible.
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Encode an EBML data size in exactly `len` bytes.
pub fn ebml_size(v: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    for (i, b) in out.iter_mut().enumerate() {
        *b = (v >> (8 * (len - 1 - i))) as u8;
    }
    out[0] |= 1 << (8 - len);
    out
}

/// Length of the EBML vint starting with `first`.
pub fn vint_len(first: u8) -> usize {
    first.leading_zeros() as usize + 1
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
