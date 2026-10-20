//! Shared helpers for the integration tests.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};

use flux_catalog::{Catalog, CatalogConfig, FileFacts, FileUpsert, ProbeUpdate, ScanBatch};
use flux_core::api::{CreateLibrary, Id, Library};
use flux_core::{ExtraKind, GuessKind, LibraryKind, NameGuess, ProbeResult, ProviderIds};

pub struct Env {
    pub _dir: tempfile::TempDir,
    pub cat: Catalog,
    pub lib: Library,
    pub root: Id,
}

pub fn env(kind: LibraryKind) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let cat = Catalog::open(dir.path().join("catalog.db"), CatalogConfig::default()).unwrap();
    let lib = cat
        .writer()
        .create_library(CreateLibrary {
            name: "Lib".into(),
            kind,
            roots: vec!["/media/lib/".into()],
        })
        .wait()
        .unwrap();
    let root = cat
        .read_blocking(|c| Ok(flux_catalog::scan_support::list_roots(c)?[0].id))
        .unwrap();
    Env {
        _dir: dir,
        cat,
        lib,
        root,
    }
}

static INO: AtomicU64 = AtomicU64::new(1000);

/// Synthetic stat facts with a unique inode and content fingerprint.
pub fn facts(path: &str) -> FileFacts {
    let ino = INO.fetch_add(1, Ordering::Relaxed);
    FileFacts {
        rel_path: path.into(),
        dev: Some(1),
        ino: Some(ino),
        btime_ns: Some(1_600_000_000_000_000_000),
        size: 1_000_000 + ino,
        mtime_ns: 1_700_000_000_000_000_000,
        fp128: Some(u128::from(ino) * 0x9e37_79b9_7f4a_7c15),
        oshash: Some(ino),
    }
}

pub fn upsert(path: &str, guess: NameGuess, probe: Option<ProbeResult>) -> FileUpsert {
    FileUpsert {
        facts: facts(path),
        probe: probe
            .map(|p| ProbeUpdate::Probed(Box::new(p)))
            .unwrap_or_default(),
        guess: Some(guess),
    }
}

pub fn movie(title: &str, year: Option<u16>) -> NameGuess {
    let mut g = NameGuess::unknown();
    g.kind = GuessKind::Movie;
    g.title = Some(title.into());
    g.year = year;
    g.confidence = 0.9;
    g.evidence = vec!["test".into()];
    g
}

pub fn movie_tmdb(title: &str, year: Option<u16>, tmdb: u64) -> NameGuess {
    let mut g = movie(title, year);
    g.ids = ProviderIds {
        tmdb: Some(tmdb),
        imdb: None,
        tvdb: None,
    };
    g
}

pub fn episode(show: &str, year: Option<u16>, season: u16, eps: &[u16]) -> NameGuess {
    let mut g = NameGuess::unknown();
    g.kind = GuessKind::Episode;
    g.title = Some(show.into());
    g.year = year;
    g.season = Some(season);
    g.episodes = eps.to_vec();
    g.confidence = 0.9;
    g
}

pub fn extra(parent: Option<&str>, year: Option<u16>, kind: ExtraKind) -> NameGuess {
    let mut g = NameGuess::unknown();
    g.kind = GuessKind::Extra;
    g.title = parent.map(str::to_string);
    g.year = year;
    g.extra = Some(kind);
    g.confidence = 0.8;
    g
}

pub fn sample() -> NameGuess {
    let mut g = NameGuess::unknown();
    g.kind = GuessKind::Sample;
    g
}

pub fn seed(name: &str) -> ProbeResult {
    flux_catalog::synth::seed_probes().remove(name).unwrap()
}

pub fn with_size(mut p: ProbeResult, w: u32, h: u32) -> ProbeResult {
    for t in p
        .tracks
        .iter_mut()
        .filter(|t| t.kind == flux_core::TrackKind::Video)
    {
        t.width = Some(w);
        t.height = Some(h);
    }
    p
}

impl Env {
    pub fn apply(&self, upserts: Vec<FileUpsert>) -> flux_catalog::ApplyOutcome {
        self.cat
            .writer()
            .apply(ScanBatch {
                root_id: self.root,
                upserts,
                ..Default::default()
            })
            .wait()
            .unwrap()
    }

    pub fn batch(&self, b: ScanBatch) -> flux_catalog::ApplyOutcome {
        self.cat.writer().apply(b).wait().unwrap()
    }

    pub fn top(&self) -> Vec<flux_core::api::ItemSummary> {
        let lib = self.lib.id;
        self.cat
            .read_blocking(|c| {
                flux_catalog::read::items_page(
                    c,
                    &flux_catalog::ItemsQuery {
                        library_id: Some(lib),
                        limit: 500,
                        ..Default::default()
                    },
                )
            })
            .unwrap()
            .items
    }

    pub fn detail(&self, id: Id) -> flux_core::api::ItemDetail {
        self.cat
            .read_blocking(|c| flux_catalog::read::item_detail(c, id, None))
            .unwrap()
            .unwrap()
    }

    pub fn children(&self, id: Id) -> Vec<flux_core::api::ItemSummary> {
        self.cat
            .read_blocking(|c| flux_catalog::read::children(c, id, None))
            .unwrap()
    }

    pub fn file_id(&self, path: &str) -> Id {
        let root = self.root;
        let p = path.to_string();
        self.cat
            .read_blocking(move |c| Ok(flux_catalog::scan_support::load_root_snapshot(c, root)?[&p].id))
            .unwrap()
    }

    pub fn find_top(&self, title: &str) -> flux_core::api::ItemSummary {
        self.top()
            .into_iter()
            .find(|i| i.title == title)
            .unwrap_or_else(|| panic!("no top-level item {title:?}"))
    }
}
