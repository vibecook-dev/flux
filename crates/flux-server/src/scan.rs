//! The scanner as the server sees it: [`ScanService`] over `flux_scan::Scanner`,
//! plus a disabled stand-in (`--no-scan`) and whatever tests plug in.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flux_catalog::Catalog;
use flux_core::api::{Event, Id, ScanStatus};
pub use flux_scan::{Priority, ScanMode};
use tokio::sync::broadcast;

/// What the server needs from the scan orchestrator.
pub trait ScanService: Send + Sync + 'static {
    /// Scan progress / item changes / availability for the SSE bus (`None`
    /// when scanning is disabled).
    fn subscribe(&self) -> Option<broadcast::Receiver<Event>>;
    /// Live status of a library scanned by this process; `None` = use the
    /// catalog's stored status.
    fn status(&self, library_id: Id) -> Option<ScanStatus>;
    /// Starts a scan (or joins the running one). `Err` explains why scanning
    /// is not possible (shown to the user, 503).
    fn scan_library(&self, library_id: Id, mode: ScanMode, priority: Priority) -> Result<(), String>;
    /// Follows filesystem changes of the library's roots. Idempotent; the
    /// first call per library also catches up with changes made while
    /// nothing watched (flux-scan: a Quick scan at Scheduled priority).
    fn watch(&self, library_id: Id);
    fn shutdown(&self, timeout: Duration);
    /// Puts the live per-root `last_seen` (when each root last answered a
    /// scan of this process, even one that wrote nothing) over the
    /// catalog's. Nothing by default.
    fn overlay_roots(&self, _lib: &mut flux_core::api::Library) {}
}

/// The real orchestrator.
pub struct FluxScanner {
    inner: flux_scan::Scanner,
}

impl ScanService for FluxScanner {
    fn subscribe(&self) -> Option<broadcast::Receiver<Event>> {
        Some(self.inner.subscribe())
    }

    fn status(&self, library_id: Id) -> Option<ScanStatus> {
        let s = self.inner.status(library_id);
        // "Idle with zero counts if it was never scanned by this process":
        // the catalog's stored status says more then.
        (s.started_at.is_some() || s.state != flux_core::api::ScanState::Idle).then_some(s)
    }

    fn scan_library(&self, library_id: Id, mode: ScanMode, priority: Priority) -> Result<(), String> {
        let _handle = self.inner.scan_library(library_id, mode, priority);
        Ok(())
    }

    fn watch(&self, library_id: Id) {
        if let Err(e) = self.inner.watch(library_id) {
            tracing::warn!(library_id, error = %e, "cannot watch library for changes");
        }
    }

    fn shutdown(&self, timeout: Duration) {
        self.inner.shutdown(timeout);
    }

    fn overlay_roots(&self, lib: &mut flux_core::api::Library) {
        self.inner.overlay_roots(lib);
    }
}

/// Scanning switched off; libraries show their stored status.
pub struct NoScanner {
    reason: String,
}

impl NoScanner {
    pub fn new(reason: impl Into<String>) -> Self {
        NoScanner {
            reason: reason.into(),
        }
    }
}

impl ScanService for NoScanner {
    fn subscribe(&self) -> Option<broadcast::Receiver<Event>> {
        None
    }
    fn status(&self, _library_id: Id) -> Option<ScanStatus> {
        None
    }
    fn scan_library(&self, _library_id: Id, _mode: ScanMode, _priority: Priority) -> Result<(), String> {
        Err(self.reason.clone())
    }
    fn watch(&self, _library_id: Id) {}
    fn shutdown(&self, _timeout: Duration) {}
}

/// The scanner for a server: flux-scan, or [`NoScanner`] when scanning is
/// disabled (`--no-scan`). `ffprobe` is the binary the server resolved
/// (`--ffprobe`, the config file, next to ffmpeg, Homebrew), so the scan's
/// fallback for containers flux-probe can't read uses it too (review ROB-3).
pub fn start(
    catalog: Arc<Catalog>,
    enabled: bool,
    ffprobe: PathBuf,
) -> (Arc<dyn ScanService>, Option<broadcast::Receiver<Event>>) {
    if !enabled {
        return (
            Arc::new(NoScanner::new(
                "Scanning is turned off on this server (--no-scan)",
            )),
            None,
        );
    }
    let cfg = flux_scan::ScanConfig {
        ffprobe,
        ..flux_scan::ScanConfig::default()
    };
    let scanner = flux_scan::Scanner::new(catalog, cfg);
    let rx = scanner.subscribe();
    (Arc::new(FluxScanner { inner: scanner }), Some(rx))
}
