//! Content fingerprints, computed from the same pages the probe reads.
//!
//! - `fp128 = xxh3_128(size_le ‖ first 64 KiB ‖ last 64 KiB)` (research/02
//!   §4.2). It survives renames, cross-volume moves, copies and restores, and
//!   is what the catalog re-attaches tombstoned files by.
//! - `oshash` (OpenSubtitles hash) = size + Σ little-endian u64 words of the
//!   first and last 64 KiB, wrapping. Defined only for files ≥ 128 KiB.
//!
//! Both read through the caller's [`PageCache`], which then serves the
//! probe's first read (and usually its tail read) without new I/O.

use std::io;

use flux_probe::{PageCache, RangeRead};
use xxhash_rust::xxh3::Xxh3;

/// Bytes hashed at each end of the file.
pub const EDGE: u64 = 64 * 1024;

/// Fingerprints of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub fp128: u128,
    pub oshash: Option<u64>,
}

/// Fingerprint the file behind `cache` (size from the source).
pub fn fingerprint<R: RangeRead>(cache: &mut PageCache<R>) -> io::Result<Fingerprint> {
    let size = cache.size();
    let head = cache.read(0, EDGE.min(size))?;
    let tail_start = size.saturating_sub(EDGE);
    let tail = cache.read(tail_start, size - tail_start)?;
    Ok(Fingerprint {
        fp128: fp128_of(size, &head, &tail),
        oshash: oshash_of(size, &head, &tail),
    })
}

/// [`fingerprint`] without a page cache, for the background pass (nothing
/// else reads the file there): the head, then the tail, read straight into
/// one 64 KiB buffer on the stack and hashed as they come. Same values as
/// [`fingerprint`]. The cached path costs ~384 KiB of short-lived buffers
/// per file (span, pages, copy out, twice); this one allocates nothing.
pub fn fingerprint_direct<R: RangeRead>(src: &mut R) -> io::Result<Fingerprint> {
    let size = src.size();
    let mut buf = [0u8; EDGE as usize];
    let mut h = Xxh3::new();
    h.update(&size.to_le_bytes());
    let head = read_up_to(src, 0, &mut buf[..EDGE.min(size) as usize])?;
    h.update(&buf[..head]);
    let head_sum = (head as u64 == EDGE).then(|| word_sum(&buf));
    let tail_start = size.saturating_sub(EDGE);
    let tail = read_up_to(src, tail_start, &mut buf[..(size - tail_start) as usize])?;
    h.update(&buf[..tail]);
    let tail_sum = (tail as u64 == EDGE).then(|| word_sum(&buf));
    let oshash = match (head_sum, tail_sum) {
        (Some(a), Some(b)) if size >= 2 * EDGE => Some(size.wrapping_add(a).wrapping_add(b)),
        _ => None,
    };
    Ok(Fingerprint {
        fp128: h.digest128(),
        oshash,
    })
}

/// Fills `buf` from `off` until it is full or the source ends (a file that
/// shrank under us); returns the bytes read, like `PageCache::read`.
fn read_up_to<R: RangeRead>(src: &mut R, off: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = src.read_at(off + filled as u64, &mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Σ little-endian u64 words, wrapping (the OpenSubtitles hash's sum).
fn word_sum(b: &[u8]) -> u64 {
    b.as_chunks::<8>()
        .0
        .iter()
        .fold(0u64, |acc, w| acc.wrapping_add(u64::from_le_bytes(*w)))
}

/// `xxh3_128(size_le ‖ head ‖ tail)`.
pub fn fp128_of(size: u64, head: &[u8], tail: &[u8]) -> u128 {
    let mut h = Xxh3::new();
    h.update(&size.to_le_bytes());
    h.update(head);
    h.update(tail);
    h.digest128()
}

/// The OpenSubtitles hash, or `None` below 128 KiB (where it is undefined).
pub fn oshash_of(size: u64, head: &[u8], tail: &[u8]) -> Option<u64> {
    if size < 2 * EDGE || head.len() as u64 != EDGE || tail.len() as u64 != EDGE {
        return None;
    }
    Some(size.wrapping_add(word_sum(head)).wrapping_add(word_sum(tail)))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<u8>);
    impl RangeRead for Mem {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
            let off = off as usize;
            if off >= self.0.len() {
                return Ok(0);
            }
            let n = buf.len().min(self.0.len() - off);
            buf[..n].copy_from_slice(&self.0[off..off + n]);
            Ok(n)
        }
    }

    fn data(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn fingerprint_covers_size_head_and_tail_and_reuses_pages() {
        let bytes = data(1_000_000, 7);
        let mut cache = PageCache::new(Mem(bytes.clone()));
        let fp = fingerprint(&mut cache).unwrap();
        let reads = cache.reads;
        assert!(reads <= 2, "head + tail = at most 2 reads, got {reads}");
        let head = &bytes[..65536];
        let tail = &bytes[bytes.len() - 65536..];
        assert_eq!(fp.fp128, fp128_of(1_000_000, head, tail));
        assert!(fp.oshash.is_some());
        // The probe's first page is already cached: no new read.
        let again = cache.read(0, 65536).unwrap();
        assert_eq!(again, head);
        assert_eq!(cache.reads, reads);

        // A change in the middle is invisible (by design), at either end not.
        let mut mid = bytes.clone();
        mid[500_000] ^= 1;
        let mut c2 = PageCache::new(Mem(mid));
        assert_eq!(fingerprint(&mut c2).unwrap().fp128, fp.fp128);
        let mut end = bytes.clone();
        let last = end.len() - 1;
        end[last] ^= 1;
        let mut c3 = PageCache::new(Mem(end));
        assert_ne!(fingerprint(&mut c3).unwrap().fp128, fp.fp128);
        // Same bytes, different size → different fingerprint.
        let mut longer = bytes;
        longer.push(0);
        let mut c4 = PageCache::new(Mem(longer));
        assert_ne!(fingerprint(&mut c4).unwrap().fp128, fp.fp128);
    }

    #[test]
    fn small_files_hash_whole_and_have_no_oshash() {
        let bytes = data(1000, 3);
        let mut cache = PageCache::new(Mem(bytes.clone()));
        let fp = fingerprint(&mut cache).unwrap();
        assert_eq!(fp.fp128, fp128_of(1000, &bytes, &bytes));
        assert_eq!(fp.oshash, None);
        let mut empty = PageCache::new(Mem(Vec::new()));
        assert_eq!(fingerprint(&mut empty).unwrap().fp128, fp128_of(0, &[], &[]));
    }

    /// The background pass's direct read gives exactly the cached path's
    /// values, at every size class (empty, below one edge, between one and
    /// two edges, exactly two, large), and on a source that returns short
    /// reads.
    #[test]
    fn direct_matches_cached() {
        struct Short(Vec<u8>);
        impl RangeRead for Short {
            fn size(&self) -> u64 {
                self.0.len() as u64
            }
            fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
                let off = off as usize;
                if off >= self.0.len() {
                    return Ok(0);
                }
                let n = buf.len().min(self.0.len() - off).min(1000);
                buf[..n].copy_from_slice(&self.0[off..off + n]);
                Ok(n)
            }
        }
        for n in [
            0usize, 1, 1000, 65_535, 65_536, 65_537, 100_000, 131_071, 131_072, 131_073, 1_000_000,
        ] {
            let bytes = data(n, 5);
            let cached = fingerprint(&mut PageCache::new(Mem(bytes.clone()))).unwrap();
            assert_eq!(
                fingerprint_direct(&mut Mem(bytes.clone())).unwrap(),
                cached,
                "size {n}"
            );
            assert_eq!(
                fingerprint_direct(&mut Short(bytes)).unwrap(),
                cached,
                "size {n}, short reads"
            );
        }
    }

    #[test]
    fn oshash_matches_reference_definition() {
        // Reference: size + sum of u64 LE words of first and last 64 KiB.
        let bytes = data(300_000, 11);
        let words = |b: &[u8]| -> u64 {
            let mut s = 0u64;
            for i in (0..b.len()).step_by(8) {
                s = s.wrapping_add(u64::from_le_bytes(b[i..i + 8].try_into().unwrap()));
            }
            s
        };
        let want = 300_000u64
            .wrapping_add(words(&bytes[..65536]))
            .wrapping_add(words(&bytes[bytes.len() - 65536..]));
        let mut cache = PageCache::new(Mem(bytes));
        assert_eq!(fingerprint(&mut cache).unwrap().oshash, Some(want));
    }
}
