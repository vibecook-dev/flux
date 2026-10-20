//! Fill a flux data dir's catalog without flux-scan: walk folders, name the
//! files with flux-names, probe them with flux-probe and apply the results
//! with the catalog writer (what flux-scan will do, minus fingerprints,
//! moves, watching and I/O admission). For running the server before
//! flux-scan lands, and for benchmarks.
//!
//! ```text
//! cargo run --release -p flux-server --example seed_catalog -- \
//!     --data-dir "$TMPDIR/flux-demo" \
//!     Movies:movies:"$FLUX_TEST_MEDIA/library-tiny/Movies" \
//!     TV:shows:"$FLUX_TEST_MEDIA/library-tiny/TV"
//! flux-server --data-dir "$TMPDIR/flux-demo" --no-scan
//! ```
//!
//! Libraries that already exist (by name) are skipped.

#[path = "../tests/common/mod.rs"]
#[allow(unused_imports)]
mod common;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use flux_catalog::{read, Catalog, CatalogConfig};
use flux_server::config::LibrarySpec;

fn main() -> Result<()> {
    let mut data_dir: Option<PathBuf> = None;
    let mut specs: Vec<LibrarySpec> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(args.next().context("--data-dir DIR")?)),
            s => specs.push(s.parse().map_err(anyhow::Error::msg)?),
        }
    }
    let Some(data_dir) = data_dir else {
        bail!("usage: seed_catalog --data-dir DIR NAME:KIND:PATH...");
    };
    std::fs::create_dir_all(&data_dir)?;
    let cat = Catalog::open(data_dir.join("catalog.db"), CatalogConfig::default())?;
    let existing = cat.read_blocking(read::list_libraries)?;
    for s in specs {
        if existing.iter().any(|l| l.name == s.name) {
            eprintln!("{}: exists, skipped", s.name);
            continue;
        }
        let t0 = Instant::now();
        let id = common::seed(&cat, &s.name, s.kind, Path::new(&s.path), |_| true);
        let lib = cat
            .read_blocking(move |c| read::library(c, id))?
            .context("library")?;
        eprintln!(
            "{}: library {} with {} items in {:.2} s",
            s.name,
            id,
            lib.item_count,
            t0.elapsed().as_secs_f64()
        );
    }
    cat.close()?;
    Ok(())
}
