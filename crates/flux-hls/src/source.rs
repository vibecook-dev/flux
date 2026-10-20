//! Positioned reads from a media file, with I/O accounting.
//!
//! A [`Source`] does not pin its file. The descriptor is opened on demand and
//! closed again by a background reaper once the source has been idle for
//! [`idle_timeout`] (30 s by default), or as soon as the file is deleted or
//! replaced at its path. A prepared asset in a cache must not keep a deleted
//! film's blocks allocated, nor stop an external drive from ejecting
//! (review finding PB-4). A closed source reopens on its next read and checks
//! that the file is still the one it was opened as (inode, size and mtime); a
//! file that changed fails the read instead of serving bytes the index doesn't
//! describe. The inode matters for a path that is a symlink: re-pointed at
//! another file with the same size and mtime, it must not be read as the
//! file the server checked when the asset was loaded (review SEC-2).

use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Identity of a source file, used to invalidate cached indexes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceId {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
}

/// Default for [`idle_timeout`].
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the reaper looks at open files.
const REAP_TICK: Duration = Duration::from_millis(500);

static IDLE_MS: AtomicU64 = AtomicU64::new(DEFAULT_IDLE_TIMEOUT.as_millis() as u64);

/// How long an unused source keeps its file descriptor (process-wide).
pub fn idle_timeout() -> Duration {
    Duration::from_millis(IDLE_MS.load(Ordering::Relaxed))
}

/// Sets [`idle_timeout`] for every source in the process (the engine sets it
/// from `EngineConfig::source_idle_timeout`).
pub fn set_idle_timeout(d: Duration) {
    IDLE_MS.store(d.as_millis().max(1) as u64, Ordering::Relaxed);
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// `(dev, ino, nlink)` of a file: what tells "the same file" from "another
/// file at the same path".
#[cfg(unix)]
fn file_ids(m: &std::fs::Metadata) -> (u64, u64, u64) {
    (m.dev(), m.ino(), m.nlink())
}

/// Not Unix: std has no stable device/file index (Windows), so every file
/// reads as `(0, 0, 1)` and identity is the size and mtime alone. flux is
/// not supported there (docs/ENCODERS.md "Platforms"); this keeps it
/// compiling.
#[cfg(not(unix))]
fn file_ids(_m: &std::fs::Metadata) -> (u64, u64, u64) {
    (0, 0, 1)
}

/// A positioned read of exactly `buf.len()` bytes.
#[cfg(unix)]
fn read_exact_at(f: &File, buf: &mut [u8], pos: u64) -> io::Result<()> {
    f.read_exact_at(buf, pos)
}

#[cfg(windows)]
fn read_exact_at(f: &File, mut buf: &mut [u8], mut pos: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, pos) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                pos += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Milliseconds since the first use of this module (a monotonic clock).
fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// The open descriptor and the inode it refers to.
#[derive(Debug)]
struct Open {
    file: Arc<File>,
    dev: u64,
    ino: u64,
}

#[derive(Debug)]
struct Inner {
    id: SourceId,
    /// The inode of the first open: every reopen must find the same file.
    ino: u64,
    file: Mutex<Option<Open>>,
    last_used_ms: AtomicU64,
    /// This source's idle timeout in ms (0 = the process-wide one).
    idle_ms: AtomicU64,
    reads: AtomicU64,
    bytes: AtomicU64,
}

impl Inner {
    /// Closes the descriptor when idle, deleted or replaced. Returns true
    /// while it stays open.
    fn reap(&self, now: u64, check_path: bool) -> bool {
        let Ok(mut g) = self.file.lock() else {
            return false;
        };
        let Some(open) = g.as_ref() else {
            return false;
        };
        let limit = match self.idle_ms.load(Ordering::Relaxed) {
            0 => IDLE_MS.load(Ordering::Relaxed),
            ms => ms,
        };
        let idle = now.saturating_sub(self.last_used_ms.load(Ordering::Relaxed)) >= limit;
        // Unlinked: the descriptor is the last thing keeping the blocks.
        let deleted = open.file.metadata().map(|m| file_ids(&m).2 == 0).unwrap_or(true);
        // Replaced (renamed over): the path now names another inode.
        let replaced = check_path
            && !std::fs::metadata(&self.id.path).is_ok_and(|m| {
                let (dev, ino, _) = file_ids(&m);
                (dev, ino) == (open.dev, open.ino)
            });
        if idle || deleted || replaced {
            // In-flight reads hold their own `Arc<File>`; the descriptor
            // closes when the last of them finishes.
            *g = None;
            return false;
        }
        true
    }
}

/// Sources with an open descriptor, watched by one reaper thread.
fn registry() -> &'static Mutex<Vec<Weak<Inner>>> {
    static REG: OnceLock<Mutex<Vec<Weak<Inner>>>> = OnceLock::new();
    REG.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("flux-source-reaper".into())
            .spawn(reaper);
        if let Err(e) = spawned {
            tracing::warn!(%e, "no source reaper thread: files stay open until dropped");
        }
        Mutex::new(Vec::new())
    })
}

fn reaper() {
    let mut tick = 0u64;
    loop {
        std::thread::sleep(REAP_TICK);
        tick += 1;
        // Path lookups can be slow on a network share: every 2 s.
        let check_path = tick.is_multiple_of(4);
        let live: Vec<Arc<Inner>> = match registry().lock() {
            Ok(mut r) => {
                r.retain(|w| w.strong_count() > 0);
                r.iter().filter_map(Weak::upgrade).collect()
            }
            Err(_) => return,
        };
        let now = now_ms();
        let closed: Vec<*const Inner> = live
            .iter()
            .filter(|i| !i.reap(now, check_path))
            .map(Arc::as_ptr)
            .collect();
        if !closed.is_empty() {
            if let Ok(mut r) = registry().lock() {
                r.retain(|w| !closed.contains(&w.as_ptr()));
            }
        }
    }
}

fn watch(inner: &Arc<Inner>) {
    if let Ok(mut r) = registry().lock() {
        let p = Arc::as_ptr(inner);
        if !r.iter().any(|w| w.as_ptr() == p) {
            r.push(Arc::downgrade(inner));
        }
    }
}

/// A media file. All reads are positioned (`pread`), so one `Source` can be
/// shared by any number of concurrent segment requests.
#[derive(Debug)]
pub struct Source {
    inner: Arc<Inner>,
}

impl Source {
    /// Opens `path` and records its identity (size, mtime). The descriptor
    /// stays open until the source is idle (see the module docs).
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let meta = file.metadata()?;
        let (dev, ino, _) = file_ids(&meta);
        let inner = Arc::new(Inner {
            id: SourceId {
                path: path.to_path_buf(),
                size: meta.len(),
                mtime_ns: mtime_ns(&meta),
            },
            ino,
            file: Mutex::new(Some(Open {
                file: Arc::new(file),
                dev,
                ino,
            })),
            last_used_ms: AtomicU64::new(now_ms()),
            idle_ms: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        });
        watch(&inner);
        Ok(Source { inner })
    }

    /// File identity (path, size, mtime).
    pub fn id(&self) -> &SourceId {
        &self.inner.id
    }

    /// The inode of the file this source reads (fixed at [`Source::open`]).
    pub fn ino(&self) -> u64 {
        self.inner.ino
    }

    /// File length in bytes.
    pub fn len(&self) -> u64 {
        self.inner.id.size
    }

    /// True for an empty file.
    pub fn is_empty(&self) -> bool {
        self.inner.id.size == 0
    }

    /// Overrides [`idle_timeout`] for this source.
    pub fn set_idle_timeout(&self, d: Duration) {
        self.inner
            .idle_ms
            .store(d.as_millis().max(1) as u64, Ordering::Relaxed);
    }

    /// True while the file descriptor is open.
    pub fn is_open(&self) -> bool {
        self.inner.file.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Closes the descriptor now (it reopens on the next read).
    pub fn close(&self) {
        if let Ok(mut g) = self.inner.file.lock() {
            *g = None;
        }
    }

    /// Opens the descriptor now if it is closed, checking that the path
    /// still names the file this source was opened as (see the module docs),
    /// and that the file hasn't shrunk below the length recorded at open (an
    /// overwrite in progress): reads past its end would fail mid-response.
    /// A file that grew (still being copied) is fine.
    pub fn ensure_open(&self) -> io::Result<()> {
        let len = self.file()?.metadata()?.len();
        if len < self.inner.id.size {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} shrank since it was opened ({len} of {} bytes)",
                    self.inner.id.path.display(),
                    self.inner.id.size
                ),
            ));
        }
        Ok(())
    }

    /// The open descriptor, reopening (and re-checking identity) if needed.
    fn file(&self) -> io::Result<Arc<File>> {
        let inner = &self.inner;
        inner.last_used_ms.store(now_ms(), Ordering::Relaxed);
        let mut g = inner
            .file
            .lock()
            .map_err(|_| io::Error::other("source lock poisoned"))?;
        if let Some(o) = g.as_ref() {
            return Ok(o.file.clone());
        }
        let file = File::open(&inner.id.path)?;
        let meta = file.metadata()?;
        let (dev, ino, _) = file_ids(&meta);
        if ino != inner.ino || meta.len() != inner.id.size || mtime_ns(&meta) != inner.id.mtime_ns {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} changed since it was opened", inner.id.path.display()),
            ));
        }
        let file = Arc::new(file);
        *g = Some(Open {
            file: file.clone(),
            dev,
            ino,
        });
        drop(g);
        watch(inner);
        Ok(file)
    }

    /// Reads up to `len` bytes at `pos`; shorter only at end of file.
    pub fn read_at(&self, pos: u64, len: usize) -> io::Result<Vec<u8>> {
        let len = len.min(self.inner.id.size.saturating_sub(pos) as usize);
        let mut buf = vec![0u8; len];
        self.read_into(pos, &mut buf)?;
        Ok(buf)
    }

    /// Fills `buf` from `pos`; errors with `UnexpectedEof` if the file is shorter.
    pub fn read_into(&self, pos: u64, buf: &mut [u8]) -> io::Result<()> {
        self.inner.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        let f = self.file()?;
        let r = read_exact_at(&f, buf, pos);
        self.inner.last_used_ms.store(now_ms(), Ordering::Relaxed);
        r
    }

    /// Like [`Source::read_at`] but returns shared bytes.
    pub fn read_bytes(&self, pos: u64, len: usize) -> io::Result<Bytes> {
        self.read_at(pos, len).map(Bytes::from)
    }

    /// (read calls, bytes read) since the source was opened.
    pub fn io_stats(&self) -> (u64, u64) {
        (
            self.inner.reads.load(Ordering::Relaxed),
            self.inner.bytes.load(Ordering::Relaxed),
        )
    }
}

/// A sliding read window over a file region, for walking element headers
/// without one syscall per header. Windows that were read stay available so
/// payloads can be sliced from memory instead of re-read.
pub(crate) struct Windows<'a> {
    src: &'a Source,
    chunk: usize,
    windows: Vec<(u64, Bytes)>,
    bytes_read: u64,
    retain: bool,
}

impl<'a> Windows<'a> {
    /// Keeps every window read (bounded segment reads; payloads are sliced later).
    pub(crate) fn new(src: &'a Source, chunk: usize) -> Self {
        Windows {
            src,
            chunk: chunk.max(4096),
            windows: Vec::new(),
            bytes_read: 0,
            retain: true,
        }
    }

    /// Keeps only the latest window (whole-file scans).
    pub(crate) fn streaming(src: &'a Source, chunk: usize) -> Self {
        Windows {
            retain: false,
            ..Self::new(src, chunk)
        }
    }

    /// Sets the size of the next read (e.g. the whole expected segment range).
    pub(crate) fn set_chunk(&mut self, chunk: usize) {
        self.chunk = chunk.max(4096);
    }

    /// Returns up to `len` bytes at `pos` (fewer only at end of file).
    pub(crate) fn get(&mut self, pos: u64, len: usize) -> io::Result<&[u8]> {
        let hit = self
            .windows
            .iter()
            .rposition(|(s, b)| pos >= *s && pos + len as u64 <= *s + b.len() as u64);
        let i = match hit {
            Some(i) => i,
            None => {
                let want = len.max(self.chunk);
                let data = self.src.read_bytes(pos, want)?;
                self.bytes_read += data.len() as u64;
                if !self.retain {
                    self.windows.clear();
                }
                self.windows.push((pos, data));
                self.windows.len() - 1
            }
        };
        let (s, b) = &self.windows[i];
        let off = (pos - s) as usize;
        let end = (off + len).min(b.len());
        Ok(&b[off..end])
    }

    /// Zero-copy slice of already-read bytes, if one window covers the range.
    pub(crate) fn slice(&self, pos: u64, len: usize) -> Option<Bytes> {
        self.windows.iter().rev().find_map(|(s, b)| {
            if pos >= *s && pos + len as u64 <= *s + b.len() as u64 {
                let off = (pos - s) as usize;
                Some(b.slice(off..off + len))
            } else {
                None
            }
        })
    }

    pub(crate) fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// True when windows are kept (payload slices stay valid).
    pub(crate) fn retains(&self) -> bool {
        self.retain
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A scratch directory under `$TMPDIR` (the SSD in the dev setup).
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flux-source-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Descriptors of this process that refer to inode `ino` (macOS reports
    /// the descriptor file system's `st_dev` under /dev/fd, so the inode
    /// number alone identifies the file).
    fn fds_on(_dev: u64, ino: u64) -> usize {
        std::fs::read_dir("/dev/fd")
            .map(|rd| {
                rd.flatten()
                    .filter(|e| std::fs::metadata(e.path()).is_ok_and(|m| m.ino() == ino))
                    .count()
            })
            .unwrap_or(0)
    }

    fn wait_closed(src: &Source, dev: u64, ino: u64, within: Duration) -> bool {
        let t = Instant::now();
        while t.elapsed() < within {
            if !src.is_open() && fds_on(dev, ino) == 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// PB-4: deleting a file that a cached asset's source has open frees its
    /// space: the descriptor is closed without waiting for the idle timeout.
    #[test]
    fn deleted_file_is_closed_and_its_space_freed() {
        let dir = scratch("deleted");
        let path = dir.join("film.ts");
        std::fs::write(&path, vec![7u8; 4 << 20]).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let (dev, ino) = (meta.dev(), meta.ino());
        let src = Source::open(&path).unwrap();
        assert_eq!(src.read_at(1 << 20, 4).unwrap(), vec![7u8; 4]);
        assert!(src.is_open());
        assert_eq!(fds_on(dev, ino), 1, "the source holds one descriptor");

        std::fs::remove_file(&path).unwrap();
        // The source stays alive (as in an asset cache) but holds nothing:
        // with no descriptor and no link left, the inode's blocks are freed.
        assert!(
            wait_closed(&src, dev, ino, Duration::from_secs(5)),
            "descriptor of the deleted file still open"
        );
        // Reads now fail cleanly instead of serving the deleted file.
        assert!(src.read_at(0, 4).is_err());
        drop(src);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file replaced at the same path (renamed over) is closed too, and the
    /// source refuses to read the new file as if it were the old one.
    #[test]
    fn replaced_file_is_closed_and_not_reread() {
        let dir = scratch("replaced");
        let path = dir.join("film.mkv");
        std::fs::write(&path, vec![1u8; 1 << 20]).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let src = Source::open(&path).unwrap();
        src.read_at(0, 16).unwrap();
        let tmp = dir.join("incoming.mkv");
        std::fs::write(&tmp, vec![2u8; 2 << 20]).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        assert!(
            wait_closed(&src, meta.dev(), meta.ino(), Duration::from_secs(5)),
            "descriptor of the replaced file still open"
        );
        let e = src.read_at(0, 16).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SEC-2 (reverify): a closed source whose path is a symlink re-pointed
    /// at another file with the same size and mtime (an attacker can set
    /// both on a file they own) refuses to read it: the reopen must find the
    /// inode the source was opened as.
    #[test]
    fn a_reopen_through_a_repointed_link_refuses_another_inode() {
        let dir = scratch("repointed");
        let media = dir.join("media.mp4");
        let secret = dir.join("secret");
        std::fs::write(&media, vec![1u8; 1 << 16]).unwrap();
        std::fs::write(&secret, vec![b'S'; 1 << 16]).unwrap();
        let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(&secret)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(
            mtime_ns(&std::fs::metadata(&secret).unwrap()),
            mtime_ns(&std::fs::metadata(&media).unwrap()),
            "same size and mtime"
        );
        let link = dir.join("Film (2020).mp4");
        std::os::unix::fs::symlink(&media, &link).unwrap();
        let src = Source::open(&link).unwrap();
        assert_eq!(src.read_at(0, 4).unwrap(), vec![1u8; 4]);
        assert_eq!(src.ino(), std::fs::metadata(&media).unwrap().ino());
        src.close();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let e = src.read_at(0, 4).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        assert!(!src.is_open());
        // Pointed back at the file it was opened as, it reads again.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&media, &link).unwrap();
        assert_eq!(src.read_at(8, 4).unwrap(), vec![1u8; 4]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An idle source closes its descriptor and reopens transparently.
    #[test]
    fn idle_source_closes_and_reopens() {
        let dir = scratch("idle");
        let path = dir.join("film.mp4");
        std::fs::write(&path, (0..=255u8).cycle().take(1 << 16).collect::<Vec<_>>()).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let src = Source::open(&path).unwrap();
        assert_eq!(src.read_at(10, 2).unwrap(), vec![10, 11]);
        // Per source: engines in other tests set the process-wide value.
        src.set_idle_timeout(Duration::from_millis(300));
        assert!(
            wait_closed(&src, meta.dev(), meta.ino(), Duration::from_secs(5)),
            "idle descriptor still open"
        );
        assert_eq!(src.read_at(300, 2).unwrap(), vec![44, 45]);
        assert!(src.is_open());
        assert_eq!(src.io_stats().0, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
