//! SQLite catalog for flux: schema, a single batched writer, pooled readers
//! and the queries behind the HTTP API.
//!
//! ```text
//!   flux-scan ──ScanBatch──► Writer (1 std thread, bounded queue, ≤1000 rows / ≤50 ms txns)
//!                                │  files → assets → items (grouping rules), FTS5, availability
//!                                ▼
//!                         catalog.db (WAL, synchronous=NORMAL)
//!                                ▲
//!   flux-server ──read()──► ReadPool (query_only connections, spawn_blocking) → flux_core::api DTOs
//! ```
//!
//! - [`schema`]: tables, migrations (`user_version`), pragmas.
//! - [`group`]: grouping rules — which movie/show/season/episode/extra a file is.
//! - [`writer`]: the single writer and its typed commands ([`Writer`]).
//! - [`read`]: reader pool and API queries ([`ReadPool`], [`read::items_page`], …).
//! - [`scan_support`]: root snapshots for the scanner's in-memory diff, and the batch apply.
//! - [`facts`]: labels and badges derived from probes at scan time.
//! - [`jobs`]: the durable job queue (match, art) the server's executor polls.
//! - [`rekey`]: re-placing episode assets (anime absolute numbering), carrying user state.
//! - [`merge`]: merging two works once provider ids agree (two show folders, one show).
//!
//! ```no_run
//! # async fn demo() -> flux_catalog::Result<()> {
//! use flux_catalog::{Catalog, CatalogConfig, read};
//! use flux_core::{api::CreateLibrary, LibraryKind};
//!
//! let cat = Catalog::open("/var/lib/flux/catalog.db", CatalogConfig::default())?;
//! let lib = cat.writer().create_library(CreateLibrary {
//!     name: "Movies".into(), kind: LibraryKind::Movies, roots: vec!["/media/movies".into()],
//! }).await?;
//! let page = cat.read(move |c| read::items_page(c, &read::ItemsQuery {
//!     library_id: Some(lib.id), ..Default::default()
//! })).await?;
//! # Ok(()) }
//! ```

pub mod error;
pub mod facts;
pub(crate) mod fts;
pub mod group;
pub mod jobs;
pub mod merge;
pub mod model;
pub mod read;
pub mod rekey;
pub mod scan_support;
pub mod schema;
pub mod synth;
pub mod writer;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rusqlite::{Connection, OpenFlags};

pub use error::{CatalogError, Result};
pub use jobs::{ClaimJobs, Enqueued, Finished, JobLease, JobResult, JobSpec, JOB_ART, JOB_MATCH};
pub use model::*;
pub use read::{FileCounts, ItemSort, ItemsQuery, PooledConn, ReadPool};
pub use rekey::{EpisodeRekey, RekeyOutcome};
pub use writer::{
    release_free_memory, release_thread_memory, set_release_hook, Reply, Request, Writer, WriterConfig,
    WriterStats,
};

/// Catalog tuning.
#[derive(Debug, Clone)]
pub struct CatalogConfig {
    pub writer: WriterConfig,
    /// Number of pooled read connections.
    pub readers: usize,
    /// Page cache of the writer connection, KiB, outside scans. While a
    /// scan writes it is [`WriterConfig::bulk_cache_kib`] (see
    /// [`Writer::begin_bulk`]).
    pub writer_cache_kib: i64,
    /// Page cache per reader connection, KiB. Readers also map the file
    /// (`mmap_bytes`), so a small cache costs little.
    pub reader_cache_kib: i64,
    /// `mmap_size` for every connection, bytes.
    pub mmap_bytes: i64,
    /// WAL pages between automatic checkpoints (SQLite's default is 1000;
    /// see [`schema`] for why flux uses more).
    pub wal_autocheckpoint_pages: u32,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        CatalogConfig {
            writer: WriterConfig::default(),
            readers: 4,
            // An idle server should not hold scan-sized caches: 16 MiB
            // (64 MiB while a scan writes) and 4 MiB per reader, down from
            // 64 + 4 × 16 MiB (most of the idle RSS after a full scan).
            writer_cache_kib: 16 * 1024,
            reader_cache_kib: 4 * 1024,
            mmap_bytes: 256 * 1024 * 1024,
            wal_autocheckpoint_pages: 16_384,
        }
    }
}

/// An open catalog: the writer handle plus the reader pool.
pub struct Catalog {
    path: PathBuf,
    writer: Writer,
    readers: ReadPool,
    counts: Arc<FileCounts>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Catalog {
    /// Open (creating and migrating if needed) the catalog at `path` and
    /// start the writer thread.
    pub fn open(path: impl AsRef<Path>, cfg: CatalogConfig) -> Result<Catalog> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        schema::configure_writer(
            &conn,
            cfg.writer_cache_kib,
            cfg.mmap_bytes,
            cfg.wal_autocheckpoint_pages,
        )?;
        schema::migrate(&mut conn)?;
        let counts = Arc::new(FileCounts::default());
        let (writer, handle) = Writer::spawn(conn, cfg.writer, cfg.writer_cache_kib, counts.clone())?;
        let readers = ReadPool::new(&path, cfg.readers, cfg.reader_cache_kib, cfg.mmap_bytes);
        Ok(Catalog {
            path,
            writer,
            readers,
            counts,
            thread: Mutex::new(Some(handle)),
        })
    }

    /// Database file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The writer handle (cloneable; hand clones to the scanner).
    pub fn writer(&self) -> &Writer {
        &self.writer
    }

    /// The reader pool (cloneable).
    pub fn readers(&self) -> &ReadPool {
        &self.readers
    }

    /// Run a read query on tokio's blocking pool.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        self.readers.run(f).await
    }

    /// Run a read query on the calling thread.
    pub fn read_blocking<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.readers.with(f)
    }

    /// [`read::list_libraries`] with the per-library file counts served
    /// from a cache the writer invalidates on commit (`GET /libraries`).
    pub async fn list_libraries(&self) -> Result<Vec<flux_core::api::Library>> {
        let counts = self.counts.clone();
        self.readers
            .run(move |c| read::list_libraries_cached(c, &counts))
            .await
    }

    /// [`read::library`] with cached file counts.
    pub async fn library(&self, id: flux_core::api::Id) -> Result<Option<flux_core::api::Library>> {
        let counts = self.counts.clone();
        self.readers
            .run(move |c| read::library_cached(c, id, &counts))
            .await
    }

    /// The file-count cache behind [`Catalog::list_libraries`], for
    /// [`read::list_libraries_cached`] on a connection of your own.
    pub fn file_counts(&self) -> &Arc<FileCounts> {
        &self.counts
    }

    /// Commit pending writes, checkpoint, optimize and stop the writer.
    /// Blocks until the writer thread exits.
    pub fn close(self) -> Result<()> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&self) -> Result<()> {
        let handle = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(handle) = handle else { return Ok(()) };
        let res = self.writer.shutdown().wait();
        if handle.join().is_err() {
            return Err(CatalogError::WriterGone);
        }
        match res {
            // Another handle already shut it down: still a clean close.
            Err(CatalogError::WriterGone) | Ok(()) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown_inner() {
            tracing::warn!(error = %e, "catalog writer did not shut down cleanly");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn catalog_handles_are_shareable_across_tasks() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<super::Catalog>();
        assert_send_sync::<super::Writer>();
        assert_send_sync::<super::ReadPool>();
    }
}
