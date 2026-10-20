//! Blocking drivers for the sans-IO probers: a positional-read source behind
//! a small 64 KiB page cache, so each `Need` costs exactly one `pread` of the
//! missing pages and nothing is ever read twice.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::Path;

use flux_core::ProbeResult;

use crate::error::ProbeError;
use crate::prober::{Prober, SeiStep, Step};
use crate::sniff::ContainerHint;

/// Default page size of the cache (and the prober's minimum read).
pub const PAGE: u64 = 64 * 1024;
/// Bytes kept before the cache is reset.
const MAX_CACHED: u64 = 16 << 20;

/// A random-access byte source: a local file, an HTTP range client, a
/// Truffle stream… Only `read_at` is needed.
pub trait RangeRead {
    /// Total size in bytes.
    fn size(&self) -> u64;
    /// Read up to `buf.len()` bytes at `offset`; returns bytes read (0 = EOF).
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
}

/// A local file read with `pread` (no shared cursor, no seeks).
#[derive(Debug)]
pub struct FileSource {
    file: File,
    size: u64,
}

impl FileSource {
    /// Open `path` read-only.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(FileSource { file, size })
    }

    /// Wrap an already-open file.
    pub fn from_file(file: File) -> io::Result<Self> {
        let size = file.metadata()?.len();
        Ok(FileSource { file, size })
    }

    /// The underlying file (e.g. to set `F_NOCACHE` for cold-cache benchmarks).
    pub fn file(&self) -> &File {
        &self.file
    }
}

impl RangeRead for FileSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::read_at(&self.file, buf, offset)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::FileExt::seek_read(&self.file, buf, offset)
        }
    }
}

/// Page cache in front of a [`RangeRead`], counting real reads and bytes.
#[derive(Debug)]
pub struct PageCache<R> {
    inner: R,
    page: u64,
    pages: BTreeMap<u64, Vec<u8>>,
    /// `read_at` calls issued.
    pub reads: u32,
    /// Bytes returned by `read_at`.
    pub bytes: u64,
}

impl<R: RangeRead> PageCache<R> {
    /// Wrap a source with [`PAGE`]-sized pages.
    pub fn new(inner: R) -> Self {
        Self::with_page_size(inner, PAGE)
    }

    /// Wrap a source with `page`-sized pages (clamped to 4 KiB–1 MiB).
    pub fn with_page_size(inner: R, page: u64) -> Self {
        PageCache {
            inner,
            page: page.clamp(4096, 1 << 20),
            pages: BTreeMap::new(),
            reads: 0,
            bytes: 0,
        }
    }

    /// Size of the underlying source.
    pub fn size(&self) -> u64 {
        self.inner.size()
    }

    /// Bytes `[off, off + len)` (clipped to EOF), loading missing pages with
    /// a single read of the span between the first and last missing page.
    pub fn read(&mut self, off: u64, len: u64) -> io::Result<Vec<u8>> {
        let size = self.inner.size();
        let end = off.saturating_add(len).min(size);
        if off >= end {
            return Ok(Vec::new());
        }
        let page = self.page;
        let (p0, p1) = (off / page, (end - 1) / page);
        let missing: Vec<u64> = (p0..=p1).filter(|p| !self.pages.contains_key(p)).collect();
        if let (Some(&m0), Some(&m1)) = (missing.first(), missing.last()) {
            if (self.pages.len() + missing.len()) as u64 * page > MAX_CACHED {
                self.pages.clear();
            }
            let start = m0 * page;
            let stop = ((m1 + 1) * page).min(size);
            let mut buf = vec![0u8; (stop - start) as usize];
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = self.inner.read_at(start + filled as u64, &mut buf[filled..])?;
                self.reads += 1;
                if n == 0 {
                    break; // file shrank under us
                }
                filled += n;
            }
            self.bytes += filled as u64;
            buf.truncate(filled);
            for (i, chunk) in buf.chunks(page as usize).enumerate() {
                self.pages.insert(m0 + i as u64, chunk.to_vec());
            }
        }
        let mut out = Vec::with_capacity((end - off) as usize);
        for p in p0..=p1 {
            let Some(data) = self.pages.get(&p) else { break };
            let ps = p * page;
            let s = off.max(ps) - ps;
            let e = (end.min(ps + data.len() as u64)).saturating_sub(ps);
            if s >= e {
                break;
            }
            out.extend_from_slice(&data[s as usize..e as usize]);
        }
        Ok(out)
    }
}

/// When to run the keyframe SEI pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameSeiMode {
    /// Only when the header pass set `needs_frame_sei` (default).
    #[default]
    IfNeeded,
    /// Always for the main video track (differential testing).
    Always,
    /// Never: header pass only.
    Never,
}

/// Options for the blocking drivers.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProbeOptions {
    /// Keyframe SEI pass policy.
    pub frame_sei: FrameSeiMode,
    /// Fall back to `ffprobe` when the container is not recognised.
    pub ffprobe_fallback: bool,
    /// Read/page size in bytes; 0 means [`PAGE`] (64 KiB). Smaller (16 KiB)
    /// is faster on local SSDs, larger is better on high-latency storage.
    pub read_size: u64,
}

impl ProbeOptions {
    fn page(&self) -> u64 {
        if self.read_size == 0 {
            PAGE
        } else {
            self.read_size
        }
    }
}

/// Probe a local file: header pass, then the keyframe SEI pass if the
/// header pass says HDR static metadata is missing. Typical cost: 1–3 reads.
pub fn probe_file(path: &Path) -> Result<ProbeResult, ProbeError> {
    probe_file_with(path, &ProbeOptions::default())
}

/// [`probe_file`] with options.
pub fn probe_file_with(path: &Path, opts: &ProbeOptions) -> Result<ProbeResult, ProbeError> {
    let src = FileSource::open(path)?;
    match probe_source(src, ContainerHint::from_path(path), opts) {
        Err(ProbeError::UnknownFormat { .. }) if opts.ffprobe_fallback => crate::ffprobe::probe(path),
        r => r,
    }
}

/// Probe any [`RangeRead`] source through a fresh page cache.
pub fn probe_source<R: RangeRead>(
    src: R,
    hint: ContainerHint,
    opts: &ProbeOptions,
) -> Result<ProbeResult, ProbeError> {
    let mut cache = PageCache::with_page_size(src, opts.page());
    probe_cached(&mut cache, hint, opts)
}

/// Probe through an existing page cache (lets callers reuse pages, e.g. the
/// scanner's fingerprint reads of the first 64 KiB). I/O counters in the
/// result are the cache's deltas for this call.
pub fn probe_cached<R: RangeRead>(
    cache: &mut PageCache<R>,
    hint: ContainerHint,
    opts: &ProbeOptions,
) -> Result<ProbeResult, ProbeError> {
    let (reads0, bytes0) = (cache.reads, cache.bytes);
    let mut prober = Prober::with_read_size(cache.size(), hint, cache.page);
    let mut step = prober.start()?;
    let mut result = loop {
        match step {
            Step::Need { offset, len } => {
                let (at, data) = fetch(cache, offset, len)?;
                step = prober.feed(at, &data)?;
            }
            Step::Done(r) => break r,
        }
    };
    let run_sei = match opts.frame_sei {
        FrameSeiMode::Never => false,
        FrameSeiMode::IfNeeded => result.needs_frame_sei,
        FrameSeiMode::Always => result.main_video().is_some(),
    };
    if run_sei {
        if let Some(mut sp) = prober.into_frame_sei() {
            match drive_sei(&mut sp, cache) {
                Ok(f) => {
                    f.apply(&mut result);
                }
                Err(e) => result.warnings.push(format!("frame SEI pass: {e}")),
            }
        }
    }
    result.reads = cache.reads - reads0;
    result.bytes_read = cache.bytes - bytes0;
    Ok(result)
}

/// Answer a `Need` with whole cache pages: the prober gets everything the
/// read brought in, which saves it from asking again for neighbouring bytes.
fn fetch<R: RangeRead>(cache: &mut PageCache<R>, offset: u64, len: u32) -> io::Result<(u64, Vec<u8>)> {
    let page = cache.page;
    let start = offset / page * page;
    let end = offset.saturating_add(u64::from(len)).div_ceil(page) * page;
    Ok((start, cache.read(start, end - start)?))
}

fn drive_sei<R: RangeRead>(
    sp: &mut crate::prober::FrameSeiProber,
    cache: &mut PageCache<R>,
) -> Result<crate::prober::FrameSei, ProbeError> {
    let mut step = sp.start()?;
    loop {
        match step {
            SeiStep::Need { offset, len } => {
                let (at, data) = fetch(cache, offset, len)?;
                step = sp.feed(at, &data)?;
            }
            SeiStep::Done(f) => return Ok(f),
        }
    }
}

/// Second pass for callers that kept only the [`ProbeResult`]: re-derives the
/// keyframe location (cheap header re-read), parses the first keyframe's SEI
/// and folds mastering / CLL / HDR10+ / DoVi RPU into `result`.
pub fn probe_frame_sei(path: &Path, result: &mut ProbeResult) -> Result<(), ProbeError> {
    let mut cache = PageCache::new(FileSource::open(path)?);
    let mut prober = Prober::new(cache.size(), ContainerHint::from_path(path));
    let mut step = prober.start()?;
    while let Step::Need { offset, len } = step {
        let (at, data) = fetch(&mut cache, offset, len)?;
        step = prober.feed(at, &data)?;
    }
    let Some(mut sp) = prober.into_frame_sei() else {
        return Err(ProbeError::Unsupported(
            "no keyframe locator for this container/track".into(),
        ));
    };
    let f = drive_sei(&mut sp, &mut cache)?;
    let (reads, bytes) = (cache.reads, cache.bytes);
    f.apply(result);
    // `apply` added the SEI pass's own counters; account for the header re-read too.
    result.reads = result.reads.saturating_sub(f.reads) + reads;
    result.bytes_read = result.bytes_read.saturating_sub(f.bytes_read) + bytes;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<u8>, u32);
    impl RangeRead for Mem {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            self.1 += 1;
            let s = (offset as usize).min(self.0.len());
            let n = buf.len().min(self.0.len() - s);
            buf[..n].copy_from_slice(&self.0[s..s + n]);
            Ok(n)
        }
    }

    #[test]
    fn page_cache_reads_once() {
        let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let mut c = PageCache::new(Mem(data.clone(), 0));
        assert_eq!(c.read(10, 100).unwrap(), &data[10..110]);
        assert_eq!(c.reads, 1);
        assert_eq!(c.read(60_000, 10_000).unwrap(), &data[60_000..70_000]); // spans pages 0-1
        assert_eq!(c.reads, 2);
        assert_eq!(c.read(0, 131_072).unwrap(), &data[..131_072]);
        assert_eq!(c.reads, 2);
        assert_eq!(c.read(199_990, 100).unwrap(), &data[199_990..]);
        assert_eq!(c.read(300_000, 10).unwrap().len(), 0);
    }
}
