//! The binary's memory allocator (docs/prototype/server.md, "Phase E:
//! memory"). The library crates stay allocator-agnostic: they only call
//! `flux_catalog::release_free_memory` / `release_thread_memory`, and this
//! module registers what those do.
//!
//! With the `jemalloc` feature (the default; not on Windows):
//! - Rust allocations go to jemalloc, and so do SQLite's
//!   (`SQLITE_CONFIG_MALLOC`): macOS's allocator keeps freed small blocks
//!   resident, so a cold scan's SQLite page cache and per-file buffers
//!   stayed in the idle server's footprint (~100 MB with ~37 MB live).
//! - One arena and small thread caches (`narenas:1`, `tcache_max:1024`,
//!   `lg_tcache_nslots_mul:-2`): with jemalloc's defaults (an arena per
//!   CPU pair, 32 KiB caches) freed blocks parked in some 30 threads'
//!   caches and half-used pages added ~10 MB to every peak and idle number;
//!   measured, the smaller caches cost no scan or playback time.
//! - Dirty pages unused for 1 s are returned (`dirty_decay_ms`), without
//!   the lazy `MADV_FREE` stage, which macOS still counts in the footprint
//!   (`muzzy_decay_ms:0`). `_RJEM_MALLOC_CONF` can override all of these.
//! - jemalloc's own purging thread exists only on Linux (enabled there), so
//!   elsewhere a small thread asks for that decay every few seconds.
//! - `release_free_memory` (end of a scan, of a bulk write) and
//!   `release_thread_memory` (an idle scan worker) flush the calling
//!   thread's cache and purge every arena.
//!
//! Without it (`--no-default-features`) this module does nothing and the
//! system allocator is used, as before Phase E.

#[cfg(all(feature = "jemalloc", not(windows)))]
pub use self::jemalloc::install;

/// Nothing to set up for the system allocator.
#[cfg(not(all(feature = "jemalloc", not(windows))))]
pub fn install() {}

#[cfg(all(feature = "jemalloc", not(windows)))]
#[allow(unsafe_code)]
mod jemalloc {
    use std::ffi::{c_char, c_int, c_void, CStr};
    #[cfg(not(target_os = "linux"))]
    use std::time::Duration;

    use tikv_jemalloc_sys as je;

    #[global_allocator]
    static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

    /// A C string pointer jemalloc reads at its first allocation, before
    /// `main` (so it must be a static, not set at run time).
    #[repr(transparent)]
    pub struct Conf(*const c_char);
    // SAFETY: points to an immutable 'static C string literal.
    unsafe impl Sync for Conf {}

    #[cfg(target_os = "linux")]
    const CONF: &CStr = c"narenas:1,tcache_max:1024,lg_tcache_nslots_mul:-2,\
        dirty_decay_ms:1000,muzzy_decay_ms:0,background_thread:true";
    #[cfg(not(target_os = "linux"))]
    const CONF: &CStr = c"narenas:1,tcache_max:1024,lg_tcache_nslots_mul:-2,\
        dirty_decay_ms:1000,muzzy_decay_ms:0";

    /// jemalloc's `malloc_conf` (tikv-jemalloc-sys prefixes its symbols).
    #[export_name = "_rjem_malloc_conf"]
    pub static MALLOC_CONF: Conf = Conf(CONF.as_ptr());

    /// How often the stand-in for jemalloc's background thread asks every
    /// arena to return the dirty pages its decay time has passed for.
    #[cfg(not(target_os = "linux"))]
    const DECAY_EVERY: Duration = Duration::from_secs(5);

    /// Before anything opens a database or spawns a worker.
    pub fn install() {
        let _ = route_sqlite();
        flux_catalog::set_release_hook(release);
        start_decay_thread();
    }

    fn ctl(name: &CStr) -> c_int {
        // SAFETY: a NUL-terminated name of a mallctl that takes and returns
        // no data (all null/0).
        unsafe {
            je::mallctl(
                name.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        }
    }

    /// The calling thread's cache back to its arena, then every arena's
    /// unused pages back to the OS (4096 = `MALLCTL_ARENAS_ALL`).
    fn release() {
        ctl(c"thread.tcache.flush");
        ctl(c"arena.4096.purge");
    }

    #[cfg(not(target_os = "linux"))]
    fn start_decay_thread() {
        let spawned = std::thread::Builder::new()
            .name("flux-mem-decay".into())
            .stack_size(64 << 10)
            .spawn(|| loop {
                std::thread::sleep(DECAY_EVERY);
                ctl(c"arena.4096.decay");
            });
        if let Err(e) = spawned {
            eprintln!("flux-server: memory decay thread did not start: {e}");
        }
    }

    #[cfg(target_os = "linux")]
    fn start_decay_thread() {}

    // SQLite's allocator (`sqlite3_mem_methods`) on jemalloc. SQLite asks
    // for 8-byte alignment; jemalloc's minimum is 16.
    unsafe extern "C" fn x_malloc(n: c_int) -> *mut c_void {
        // SAFETY: plain malloc; SQLite never asks for 0 or negative sizes.
        unsafe { je::malloc(n.max(1) as usize) }
    }
    unsafe extern "C" fn x_free(p: *mut c_void) {
        // SAFETY: `p` came from x_malloc/x_realloc (or is null).
        unsafe { je::free(p) }
    }
    unsafe extern "C" fn x_realloc(p: *mut c_void, n: c_int) -> *mut c_void {
        // SAFETY: `p` came from x_malloc/x_realloc; SQLite frees, never
        // reallocs to 0.
        unsafe { je::realloc(p, n.max(1) as usize) }
    }
    unsafe extern "C" fn x_size(p: *mut c_void) -> c_int {
        // SAFETY: `p` is a live allocation of ours (SQLite's contract); its
        // usable size is under SQLite's 2 GiB allocation limit.
        unsafe { je::malloc_usable_size(p) as c_int }
    }
    unsafe extern "C" fn x_roundup(n: c_int) -> c_int {
        // SAFETY: nallocx only computes a size class (n ≥ 1).
        unsafe { je::nallocx(n.max(1) as usize, 0) as c_int }
    }
    unsafe extern "C" fn x_init(_: *mut c_void) -> c_int {
        0 // SQLITE_OK
    }
    unsafe extern "C" fn x_shutdown(_: *mut c_void) {}

    /// Whether SQLite now allocates through jemalloc.
    fn route_sqlite() -> bool {
        use rusqlite::ffi;
        let methods = ffi::sqlite3_mem_methods {
            xMalloc: Some(x_malloc),
            xFree: Some(x_free),
            xRealloc: Some(x_realloc),
            xSize: Some(x_size),
            xRoundup: Some(x_roundup),
            xInit: Some(x_init),
            xShutdown: Some(x_shutdown),
            pAppData: std::ptr::null_mut(),
        };
        // SAFETY: called first thing in `main`, before SQLite is initialized
        // (sqlite3_config is refused afterwards: SQLITE_MISUSE, harmless).
        // SQLite copies the struct before returning.
        let rc = unsafe {
            ffi::sqlite3_config(
                ffi::SQLITE_CONFIG_MALLOC,
                &methods as *const ffi::sqlite3_mem_methods,
            )
        };
        if rc != ffi::SQLITE_OK {
            eprintln!("flux-server: SQLite keeps the system allocator (sqlite3_config: {rc})");
        }
        rc == ffi::SQLITE_OK
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use rusqlite::ffi;

        /// SQLite runs on jemalloc (its size query is jemalloc's), a real
        /// workload round-trips, and the release hook is callable.
        #[test]
        fn sqlite_allocates_from_jemalloc() {
            // The only SQLite user in this test binary, so this is first.
            assert!(route_sqlite(), "sqlite3_config before SQLite's first use");
            let c = rusqlite::Connection::open_in_memory().unwrap();
            c.execute_batch(
                "CREATE TABLE t(x TEXT);
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5000)
                 INSERT INTO t SELECT printf('%0500d', i) FROM n;",
            )
            .unwrap();
            let n: i64 = c.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
            assert_eq!(n, 5000);
            // SAFETY: a live SQLite allocation, freed right after; jemalloc
            // owns it (routing asserted above).
            unsafe {
                let p = ffi::sqlite3_malloc(1000);
                assert!(!p.is_null());
                let usable = je::malloc_usable_size(p);
                assert!(usable >= 1000);
                assert_eq!(ffi::sqlite3_msize(p) as usize, usable);
                ffi::sqlite3_free(p);
            }
            drop(c);
            release();
        }
    }
}
