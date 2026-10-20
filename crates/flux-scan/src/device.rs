//! Per-device state: one Mille `ReadBoundary` (deadlines, helper pool, stuck
//! cap), a priority-ordered file-I/O semaphore, and scan admission.
//!
//! **Device class heuristic.** `statfs(2)` on the root names the filesystem
//! (`f_fstypename` on macOS/BSD, the `f_type` magic on Linux). Network and
//! userspace filesystems — `smbfs`, `nfs`, `afpfs`, `webdav`, `cifs`, `ftp`,
//! `9p`, anything FUSE (`macfuse`, `osxfuse`, `fuse-t`, `fuse.*`, `sshfs`) —
//! and any mount macOS does not flag `MNT_LOCAL` are **slow**: they get
//! `ScanConfig::slow_concurrency` reads in flight. Everything else is
//! **fast** and gets `ssd_concurrency`. `statfs` cannot tell a spinning disk
//! from an SSD (that needs IOKit / `/sys/block/*/queue/rotational`), so a
//! local HDD counts as fast; lower `ssd_concurrency` on HDD-only machines.
//!
//! **Admission.** A scan of a root waits on the root's device. It may start
//! when nothing is running there, or when its priority is higher than every
//! running scan's (Interactive > Watcher > Scheduled > Deep): a watcher batch
//! overtakes a long scheduled scan instead of queueing behind it, while two
//! scheduled scans of one disk run one after the other. Waiters are served
//! highest priority first, then in arrival order; a raised priority (a
//! joined request) takes effect at once.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use mille_core::{ReadBoundary, ReadBoundaryOptions};
use serde::Serialize;

use crate::pool::PrioritySemaphore;
use crate::ScanConfig;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How fast a device answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceClass {
    /// Local disk (SSD/NVMe, or an HDD statfs can't tell apart).
    Fast,
    /// Network share or userspace filesystem.
    Slow,
}

/// Classify a filesystem type name (`f_fstypename`, or the Linux name
/// mapped from `f_type`). `local` is the `MNT_LOCAL` flag where known.
pub fn classify(fstype: &str, local: Option<bool>) -> DeviceClass {
    let t = fstype.to_ascii_lowercase();
    let network = matches!(
        t.as_str(),
        "smbfs"
            | "nfs"
            | "nfs4"
            | "afpfs"
            | "webdav"
            | "cifs"
            | "smb2"
            | "smb3"
            | "ftp"
            | "9p"
            | "sshfs"
            | "davfs"
    ) || t.contains("fuse")
        || t.starts_with("fuse.");
    if network || local == Some(false) {
        DeviceClass::Slow
    } else {
        DeviceClass::Fast
    }
}

/// `(fstype, MNT_LOCAL)` of the filesystem holding `path`, via `statfs`.
/// Runs on a helper thread with a 2 s cap: a hung mount answers "unknown".
pub fn fs_type(path: &Path) -> Option<(String, Option<bool>)> {
    let p = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("flux-scan-statfs".into())
        .spawn(move || {
            let _ = tx.send(statfs_type(&p));
        })
        .ok()?;
    rx.recv_timeout(Duration::from_secs(2)).ok().flatten()
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
#[allow(unsafe_code)]
fn statfs_type(path: &Path) -> Option<(String, Option<bool>)> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statfs` writes into the zeroed struct we own; `c` is a valid
    // NUL-terminated path for the duration of the call.
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    // SAFETY: the kernel NUL-terminates `f_fstypename` within its 16 bytes.
    let name = unsafe { CStr::from_ptr(s.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let local = (s.f_flags as u64 & libc::MNT_LOCAL as u64) != 0;
    Some((name, Some(local)))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(unsafe_code)]
fn statfs_type(path: &Path) -> Option<(String, Option<bool>)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: as above; `statfs` fills the struct we own.
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    let name = match s.f_type as i64 {
        0x6969 => "nfs",
        0x517B => "smbfs",
        0xFF53_4D42 => "cifs",
        0xFE53_4D42 => "smb2",
        0x6573_5546 => "fuse",
        0x0102_1997 => "9p",
        0xEF53 => "ext4",
        0x5846_5342 => "xfs",
        0x9123_683E => "btrfs",
        0x2FC1_2FC1 => "zfs",
        0x0102_1994 => "tmpfs",
        0x6573_7864 => "exfat",
        _ => "other",
    };
    Some((name.to_string(), None))
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "android"
)))]
fn statfs_type(_path: &Path) -> Option<(String, Option<bool>)> {
    None
}

/// One storage device (`st_dev`).
pub(crate) struct Device {
    pub dev: u64,
    pub class: DeviceClass,
    pub fstype: String,
    /// Directory reads in flight (walk parallelism and boundary helpers),
    /// also the cap on concurrent file reads.
    pub concurrency: usize,
    pub boundary: ReadBoundary,
    pub io: PrioritySemaphore,
    pub admission: Admission,
}

/// Device registry: one [`Device`] per `st_dev`, created on first use.
#[derive(Default)]
pub(crate) struct Devices {
    map: Mutex<HashMap<u64, Arc<Device>>>,
}

impl Devices {
    pub(crate) fn get(
        &self,
        dev: u64,
        root: &Path,
        cfg: &ScanConfig,
        hook: Option<&Arc<dyn mille_core::ReadHook>>,
    ) -> Arc<Device> {
        if let Some(d) = lock(&self.map).get(&dev) {
            return d.clone();
        }
        let (fstype, local) = fs_type(root).unwrap_or_else(|| ("unknown".into(), None));
        let class = classify(&fstype, local);
        let concurrency = match class {
            DeviceClass::Fast => cfg.ssd_concurrency,
            DeviceClass::Slow => cfg.slow_concurrency,
        }
        .max(1);
        let boundary = ReadBoundary::new(ReadBoundaryOptions {
            timeout: cfg.stall,
            first_response: Some(cfg.first_response),
            concurrency,
            hook: hook.cloned(),
            ..ReadBoundaryOptions::media()
        });
        let d = Arc::new(Device {
            dev,
            class,
            fstype,
            concurrency,
            boundary,
            io: PrioritySemaphore::new(concurrency),
            admission: Admission::default(),
        });
        lock(&self.map).entry(dev).or_insert(d).clone()
    }

    pub(crate) fn snapshot(&self) -> Vec<Arc<Device>> {
        lock(&self.map).values().cloned().collect()
    }
}

#[derive(Default)]
pub(crate) struct Admission {
    st: Mutex<AdmState>,
    cv: Condvar,
}

#[derive(Default)]
struct AdmState {
    seq: u64,
    running: Vec<(u64, Arc<AtomicU8>)>,
    waiting: Vec<(u64, Arc<AtomicU8>, u64)>,
}

/// A running scan's slot on a device; released on drop.
pub(crate) struct Admitted<'a> {
    adm: &'a Admission,
    job: u64,
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        let mut st = lock(&self.adm.st);
        if let Some(i) = st.running.iter().position(|r| r.0 == self.job) {
            st.running.swap_remove(i);
        }
        drop(st);
        self.adm.cv.notify_all();
    }
}

impl Admission {
    /// Wait for a slot. `None` when `cancelled()` became true first.
    pub(crate) fn admit(
        &self,
        job: u64,
        prio: &Arc<AtomicU8>,
        cancelled: impl Fn() -> bool,
    ) -> Option<Admitted<'_>> {
        let mut st = lock(&self.st);
        st.seq += 1;
        let seq = st.seq;
        st.waiting.push((job, prio.clone(), seq));
        loop {
            if cancelled() {
                st.waiting.retain(|w| w.0 != job);
                drop(st);
                self.cv.notify_all();
                return None;
            }
            let best = st
                .waiting
                .iter()
                .max_by(|a, b| {
                    a.1.load(Ordering::Relaxed)
                        .cmp(&b.1.load(Ordering::Relaxed))
                        .then_with(|| b.2.cmp(&a.2))
                })
                .map(|w| w.0);
            let mine = prio.load(Ordering::Relaxed);
            let top_running = st.running.iter().map(|r| r.1.load(Ordering::Relaxed)).max();
            if best == Some(job) && top_running.is_none_or(|t| mine > t) {
                st.waiting.retain(|w| w.0 != job);
                st.running.push((job, prio.clone()));
                return Some(Admitted { adm: self, job });
            }
            st = self
                .cv
                .wait_timeout(st, Duration::from_millis(50))
                .map(|r| r.0)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }

    /// Wake waiters so they re-read priorities.
    pub(crate) fn poke(&self) {
        self.cv.notify_all();
    }

    /// A scan runs on this device or waits for it (background work yields).
    pub(crate) fn busy(&self) -> bool {
        let st = lock(&self.st);
        !st.running.is_empty() || !st.waiting.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_network_and_fuse_as_slow() {
        for t in [
            "smbfs",
            "nfs",
            "afpfs",
            "webdav",
            "macfuse",
            "osxfuse",
            "fuse-t",
            "fuse.sshfs",
            "cifs",
        ] {
            assert_eq!(classify(t, Some(true)), DeviceClass::Slow, "{t}");
        }
        for t in ["apfs", "hfs", "ext4", "btrfs", "exfat", "msdos", "zfs"] {
            assert_eq!(classify(t, Some(true)), DeviceClass::Fast, "{t}");
            assert_eq!(classify(t, None), DeviceClass::Fast, "{t}");
        }
        // A non-local mount is slow whatever its name.
        assert_eq!(classify("apfs", Some(false)), DeviceClass::Slow);
    }

    #[test]
    fn statfs_names_this_volume() {
        let (t, _) = fs_type(&std::env::temp_dir()).expect("statfs works on the temp dir");
        assert!(!t.is_empty());
    }

    #[test]
    fn admission_orders_by_priority_and_lets_higher_overtake() {
        let adm = Arc::new(Admission::default());
        let low = Arc::new(AtomicU8::new(1));
        let first = adm.admit(1, &low, || false).expect("idle device admits");
        // Same priority waits behind the running scan…
        let a2 = adm.clone();
        let same = Arc::new(AtomicU8::new(1));
        let s2 = same.clone();
        let waiter = std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let _p = a2.admit(2, &s2, || false).unwrap();
            t.elapsed()
        });
        std::thread::sleep(Duration::from_millis(60));
        // …while a higher priority overtakes it at once.
        let high = Arc::new(AtomicU8::new(3));
        let h = adm.admit(3, &high, || false).expect("higher priority overtakes");
        drop(h);
        std::thread::sleep(Duration::from_millis(40));
        drop(first);
        let waited = waiter.join().unwrap();
        assert!(waited >= Duration::from_millis(90), "{waited:?}");
        // Cancellation while waiting returns None.
        let _busy = adm.admit(4, &Arc::new(AtomicU8::new(2)), || false).unwrap();
        assert!(adm.admit(5, &Arc::new(AtomicU8::new(1)), || true).is_none());
    }
}
