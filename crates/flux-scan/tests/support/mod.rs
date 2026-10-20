//! Shared helpers for the flux-scan integration tests. The corpus comes
//! from `$FLUX_TEST_MEDIA` and is only read; everything mutable happens in
//! scratch copies under `$TMPDIR/flux-scan-tests/` (APFS clones on macOS:
//! instant, no data copied).

#![allow(dead_code)]

pub mod truth;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flux_catalog::{scan_support, Catalog, CatalogConfig, FileRow};
use flux_core::api::{CreateLibrary, Id};
use flux_core::LibraryKind;
use flux_scan::{Priority, ScanConfig, ScanMode, ScanSummary, Scanner, ScannerOptions, StartupCatchUp};

/// The corpus root (`$FLUX_TEST_MEDIA`, with `library-tiny/` in it), or
/// `None` and a message.
pub fn corpus() -> Option<PathBuf> {
    flux_test_media::media("library-tiny")?;
    flux_test_media::media_root()
}

/// A scratch directory removed (permissions restored first) on drop.
pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join("flux-scan-tests")
            .join(format!("{name}-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch { dir }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&self.dir)
            .status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Clone a corpus tree (APFS clonefile; falls back to a plain copy).
pub fn clone_tree(src: &Path, dst: &Path) {
    flux_test_media::clone_tree(src, dst).expect("copy the tree");
}

pub fn open_catalog(dir: &Path) -> Arc<Catalog> {
    Arc::new(Catalog::open(dir.join("catalog.db"), CatalogConfig::default()).expect("open catalog"))
}

/// Create a one-root library; returns `(library id, root id)`.
pub fn add_library(cat: &Catalog, name: &str, kind: LibraryKind, root: &Path) -> (Id, Id) {
    let lib = cat
        .writer()
        .create_library(CreateLibrary {
            name: name.into(),
            kind,
            roots: vec![root.to_string_lossy().into_owned()],
        })
        .wait()
        .expect("create library");
    let root_id = cat
        .read_blocking(move |c| {
            Ok(scan_support::list_roots(c)?
                .into_iter()
                .find(|r| r.library_id == lib.id)
                .map(|r| r.id))
        })
        .expect("roots")
        .expect("root");
    (lib.id, root_id)
}

pub fn config() -> ScanConfig {
    ScanConfig {
        workers: 4,
        ..ScanConfig::default()
    }
}

pub fn scanner(cat: &Arc<Catalog>) -> Scanner {
    scanner_with(
        cat,
        config(),
        ScannerOptions {
            startup: StartupCatchUp::Nothing,
            ..ScannerOptions::default()
        },
    )
}

pub fn scanner_with(cat: &Arc<Catalog>, cfg: ScanConfig, opts: ScannerOptions) -> Scanner {
    Scanner::with_options(cat.clone(), cfg, opts)
}

pub fn scan(s: &Scanner, lib: Id) -> ScanSummary {
    s.scan_library(lib, ScanMode::Quick, Priority::Interactive)
        .wait()
        .expect("scan")
}

pub fn snapshot(cat: &Catalog, root_id: Id) -> std::collections::HashMap<String, FileRow> {
    cat.read_blocking(move |c| scan_support::load_root_snapshot(c, root_id))
        .expect("snapshot")
}

pub fn writer_commands(cat: &Catalog) -> u64 {
    cat.writer()
        .stats()
        .commands
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// Poll `f` until it returns `Some` or `timeout` passes.
pub fn eventually<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<(T, Duration)> {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if let Some(v) = f() {
            return Some((v, t.elapsed()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}
