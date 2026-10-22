//! The byte store behind the sans-IO prober, and the "replay" parse context.
//!
//! Design: container parsers are written as plain sequential code over a
//! [`Ctx`]. When they touch a byte range that has not been fed yet they return
//! [`PErr::Need`]; the [`crate::Prober`] turns that into a `Step::Need`, and
//! when the driver feeds the bytes the parse is simply re-run from the start
//! over the (now larger) store. Header parsing costs microseconds, so replaying
//! it 2–4 times is free, and it keeps every parser a straight line of `?`s
//! instead of a hand-written state machine per container.

use flux_core::Container;

use crate::error::ProbeError;

/// Internal parse outcome: either "feed me this range" or a real failure.
#[derive(Debug)]
pub(crate) enum PErr {
    /// The parse needs `[offset, offset + len)`, which has not been fed yet.
    Need { offset: u64, len: u64 },
    /// The parse failed for good.
    Fail(ProbeError),
}

impl From<ProbeError> for PErr {
    fn from(e: ProbeError) -> Self {
        PErr::Fail(e)
    }
}

pub(crate) type PResult<T> = Result<T, PErr>;

/// Shorthand for a fatal "malformed container" error.
pub(crate) fn malformed<T>(c: Container, msg: impl Into<String>) -> PResult<T> {
    Err(PErr::Fail(ProbeError::malformed(c, msg)))
}

/// Fed byte ranges, kept as sorted, non-overlapping, non-adjacent runs.
#[derive(Debug, Default)]
pub(crate) struct Store {
    runs: Vec<(u64, Vec<u8>)>,
    file_size: u64,
}

impl Store {
    pub(crate) fn new(file_size: u64) -> Self {
        Store {
            runs: Vec::new(),
            file_size,
        }
    }

    pub(crate) fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Total bytes held (for diagnostics/tests).
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.runs.iter().map(|(_, d)| d.len()).sum()
    }

    /// Insert fed bytes, clipped to the file size, merging with neighbours.
    pub(crate) fn insert(&mut self, off: u64, data: &[u8]) {
        if off >= self.file_size || data.is_empty() {
            return;
        }
        let max = usize::try_from(self.file_size - off).unwrap_or(usize::MAX);
        let data = &data[..data.len().min(max)];
        let end = off + data.len() as u64;
        // Runs that overlap or touch [off, end) get merged into one.
        let mut lo = off;
        let mut hi = end;
        let mut merged: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut keep: Vec<(u64, Vec<u8>)> = Vec::with_capacity(self.runs.len() + 1);
        for (ro, rd) in self.runs.drain(..) {
            let re = ro + rd.len() as u64;
            if re < off || ro > end {
                keep.push((ro, rd));
            } else {
                lo = lo.min(ro);
                hi = hi.max(re);
                merged.push((ro, rd));
            }
        }
        let mut buf = vec![0u8; usize::try_from(hi - lo).unwrap_or(0)];
        for (ro, rd) in &merged {
            let s = (ro - lo) as usize;
            buf[s..s + rd.len()].copy_from_slice(rd);
        }
        let s = (off - lo) as usize;
        buf[s..s + data.len()].copy_from_slice(data);
        keep.push((lo, buf));
        keep.sort_by_key(|(o, _)| *o);
        self.runs = keep;
    }

    /// The cached bytes `[off, off + len)` if fully present.
    pub(crate) fn get(&self, off: u64, len: u64) -> Option<&[u8]> {
        let end = off.checked_add(len)?;
        for (ro, rd) in &self.runs {
            let re = ro + rd.len() as u64;
            if *ro <= off && end <= re {
                let s = usize::try_from(off - ro).ok()?;
                let e = usize::try_from(end - ro).ok()?;
                return rd.get(s..e);
            }
        }
        None
    }

    /// All contiguous cached bytes starting at `off` (possibly empty).
    pub(crate) fn cached_from(&self, off: u64) -> &[u8] {
        for (ro, rd) in &self.runs {
            let re = ro + rd.len() as u64;
            if *ro <= off && off < re {
                return rd.get((off - ro) as usize..).unwrap_or(&[]);
            }
        }
        &[]
    }
}

/// Reads past this many feeds stop *optional* work (attachments, end-of-file
/// tags) instead of asking for more data. Keeps hostile/odd files bounded.
pub(crate) const SOFT_READ_BUDGET: u32 = 24;

/// Parse context: the store plus soft-budget bookkeeping and warnings.
pub(crate) struct Ctx<'a> {
    store: &'a Store,
    /// Feeds so far (drives the soft budget).
    pub(crate) reads: u32,
    pub(crate) container: Container,
    pub(crate) warnings: Vec<String>,
    /// Offsets of the boxes whose children ended in junk (MP4: a child that
    /// overruns its parent), recorded by walks that only borrow the context.
    junk: std::cell::RefCell<Vec<u64>>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(store: &'a Store, reads: u32) -> Self {
        Ctx {
            store,
            reads,
            container: Container::Unknown,
            warnings: Vec::new(),
            junk: Default::default(),
        }
    }

    /// Records that the children of the box at `parent` end in junk.
    pub(crate) fn note_junk(&self, parent: u64) {
        let mut j = self.junk.borrow_mut();
        if !j.contains(&parent) {
            j.push(parent);
        }
    }

    /// How many boxes' children ended in junk ([`Ctx::note_junk`]).
    pub(crate) fn junk(&self) -> usize {
        self.junk.borrow().len()
    }

    pub(crate) fn file_size(&self) -> u64 {
        self.store.file_size()
    }

    pub(crate) fn store(&self) -> &'a Store {
        self.store
    }

    /// Exactly `[off, off + len)`. Past EOF is a `Truncated` failure;
    /// not-yet-fed is a `Need`.
    pub(crate) fn bytes(&self, off: u64, len: u64) -> PResult<&'a [u8]> {
        let size = self.store.file_size();
        match off.checked_add(len) {
            Some(end) if end <= size => {}
            _ => {
                return Err(PErr::Fail(ProbeError::Truncated {
                    container: self.container,
                    offset: off,
                    needed: len,
                    file_size: size,
                }))
            }
        }
        if len == 0 {
            return Ok(&[]);
        }
        match self.store.get(off, len) {
            Some(b) => Ok(b),
            None => {
                // Ask only for the part not already cached right after `off`.
                let have = (self.store.cached_from(off).len() as u64).min(len);
                Err(PErr::Need {
                    offset: off + have,
                    len: len - have,
                })
            }
        }
    }

    /// Up to `len` bytes at `off`, clipped to EOF.
    pub(crate) fn bytes_upto(&self, off: u64, len: u64) -> PResult<&'a [u8]> {
        let size = self.store.file_size();
        if off >= size {
            return Ok(&[]);
        }
        self.bytes(off, len.min(size - off))
    }

    /// Whether optional sections may still trigger reads.
    pub(crate) fn optional_reads_ok(&self) -> bool {
        self.reads < SOFT_READ_BUDGET
    }

    pub(crate) fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }

    /// Run an optional parse step: failures become warnings, and once the
    /// soft read budget is spent, `Need`s become "skipped" warnings.
    pub(crate) fn optional<T>(&mut self, what: &str, r: PResult<T>) -> PResult<Option<T>> {
        match r {
            Ok(v) => Ok(Some(v)),
            Err(PErr::Need { .. }) if !self.optional_reads_ok() => {
                self.warn(format!("{what}: skipped (read budget)"));
                Ok(None)
            }
            Err(e @ PErr::Need { .. }) => Err(e),
            Err(PErr::Fail(e)) => {
                self.warn(format!("{what}: {e}"));
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_runs() {
        let mut s = Store::new(100);
        s.insert(10, &[1; 10]);
        s.insert(30, &[3; 10]);
        assert!(s.get(10, 10).is_some());
        assert!(s.get(15, 20).is_none());
        s.insert(20, &[2; 10]); // bridges both
        assert_eq!(s.runs.len(), 1);
        let all = s.get(10, 30).unwrap();
        assert_eq!(&all[..10], &[1; 10]);
        assert_eq!(&all[10..20], &[2; 10]);
        assert_eq!(&all[20..], &[3; 10]);
        s.insert(95, &[9; 20]); // clipped at EOF
        assert_eq!(s.get(95, 5), Some(&[9u8; 5][..]));
        assert_eq!(s.held(), 35);
    }

    #[test]
    fn ctx_bounds() {
        let mut s = Store::new(50);
        s.insert(0, &[0; 20]);
        let c = Ctx::new(&s, 0);
        assert!(c.bytes(0, 20).is_ok());
        // Only the uncached suffix is requested.
        assert!(matches!(c.bytes(10, 20), Err(PErr::Need { offset: 20, len: 10 })));
        assert!(matches!(c.bytes(30, 5), Err(PErr::Need { offset: 30, len: 5 })));
        assert!(matches!(
            c.bytes(40, 20),
            Err(PErr::Fail(ProbeError::Truncated { .. }))
        ));
        assert_eq!(c.bytes_upto(45, 20).err().map(|_| ()), Some(()));
        assert_eq!(c.bytes_upto(60, 20).unwrap().len(), 0);
    }
}
