//! "Never mistake unreachable for deleted": unavailable roots, tombstones,
//! re-attachment by fingerprint, moves, purge.

mod common;

use common::*;
use flux_catalog::{read, scan_support, FileMove, FileState, FileUpsert, ProbeUpdate, ScanBatch};
use flux_core::api::{Identity, Role};
use flux_core::LibraryKind;

#[test]
fn unavailable_root_keeps_items_and_flags_assets() {
    let e = env(LibraryKind::Mixed);
    e.apply(vec![
        upsert("Heat.mkv", movie("Heat", Some(1995)), None),
        upsert("Lost/S01E01.mkv", episode("Lost", Some(2004), 1, &[1]), None),
    ]);
    let ch = e
        .cat
        .writer()
        .set_root_availability(e.root, false)
        .wait()
        .unwrap();
    assert!(ch.changed);
    assert_eq!(ch.files_affected, 2);
    assert_eq!(ch.items_affected, 2);
    let top = e.top();
    assert_eq!(top.len(), 2, "unreachable is not deleted");
    let heat = e.find_top("Heat");
    let d = e.detail(heat.id);
    assert_eq!(d.assets.len(), 1);
    assert!(!d.assets[0].available);
    let state: String = e
        .cat
        .read_blocking(move |c| {
            Ok(c.query_row("SELECT state FROM item WHERE id = ?1", [heat.id], |r| r.get(0))?)
        })
        .unwrap();
    assert_eq!(state, "unavailable");
    let libs = e.cat.read_blocking(read::list_libraries).unwrap();
    assert_eq!(libs[0].scan.state, flux_core::api::ScanState::Unavailable);
    let loc = e
        .cat
        .read_blocking(move |c| read::asset_location(c, d.assets[0].id))
        .unwrap()
        .unwrap();
    assert!(!loc.available);

    // Repeating is a no-op; coming back restores everything.
    assert!(
        !e.cat
            .writer()
            .set_root_availability(e.root, false)
            .wait()
            .unwrap()
            .changed
    );
    e.cat.writer().set_root_availability(e.root, true).wait().unwrap();
    assert!(e.detail(heat.id).assets[0].available);
    let show = e.find_top("Lost");
    let state: String = e
        .cat
        .read_blocking(move |c| {
            Ok(c.query_row("SELECT state FROM item WHERE id = ?1", [show.id], |r| r.get(0))?)
        })
        .unwrap();
    assert_eq!(state, "available");
}

#[test]
fn a_batch_for_an_unavailable_root_brings_it_back() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert("Heat.mkv", movie("Heat", Some(1995)), None)]);
    e.cat
        .writer()
        .set_root_availability(e.root, false)
        .wait()
        .unwrap();
    e.apply(vec![upsert("Alien.mkv", movie("Alien", Some(1979)), None)]);
    let heat = e.find_top("Heat");
    assert!(e.detail(heat.id).assets[0].available);
}

#[test]
fn missing_files_tombstone_hide_and_reattach_by_fingerprint() {
    let e = env(LibraryKind::Movies);
    let up = upsert(
        "Heat (1995)/Heat (1995).mkv",
        movie("Heat", Some(1995)),
        Some(seed("h264_mkv")),
    );
    let fp = up.facts.fp128;
    let size = up.facts.size;
    e.apply(vec![up]);
    let heat = e.find_top("Heat");
    let asset = e.detail(heat.id).assets[0].id;
    let user = e
        .cat
        .writer()
        .ensure_user(Identity {
            login_name: "a".into(),
            display_name: "A".into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Viewer,
            tags: Vec::new(),
        })
        .wait()
        .unwrap();
    e.cat
        .writer()
        .put_progress(user, heat.id, Some(asset), 1000, 6021)
        .wait()
        .unwrap();

    let fid = e.file_id("Heat (1995)/Heat (1995).mkv");
    let out = e.batch(ScanBatch {
        root_id: e.root,
        missing: vec![fid],
        ..Default::default()
    });
    assert_eq!(out.files_missing, 1);
    assert_eq!(out.items_removed, vec![heat.id]);
    assert!(e.top().is_empty(), "missing items are hidden");
    let snap = e
        .cat
        .read_blocking(|c| scan_support::load_root_snapshot(c, e.root))
        .unwrap();
    assert_eq!(snap["Heat (1995)/Heat (1995).mkv"].state, FileState::Missing);

    // The same content shows up elsewhere (copied to another folder): it
    // re-attaches to the tombstoned file, keeping asset, item and progress.
    let mut moved = facts("Archive/Heat.1995.mkv");
    moved.fp128 = fp;
    moved.size = size;
    let out = e.batch(ScanBatch {
        root_id: e.root,
        upserts: vec![FileUpsert {
            facts: moved,
            probe: ProbeUpdate::Keep,
            guess: Some(movie("Heat", Some(1995))),
        }],
        ..Default::default()
    });
    assert_eq!(out.files_reattached, 1);
    assert_eq!(out.files_added, 0);
    let back = e.find_top("Heat");
    assert_eq!(back.id, heat.id);
    let d = e.detail(back.id);
    assert_eq!(d.assets[0].id, asset);
    assert!(d.assets[0].path.ends_with("Archive/Heat.1995.mkv"));
    let p = e
        .cat
        .read_blocking(move |c| read::progress(c, user, heat.id))
        .unwrap()
        .unwrap();
    assert_eq!(p.position_ms, 1000);
    let found = e
        .cat
        .read_blocking(move |c| scan_support::find_by_fp128(c, fp.unwrap(), size))
        .unwrap();
    assert_eq!(found.len(), 1);
}

#[test]
fn moves_keep_identity_and_overwrite_evicts_stale_rows() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("a/Heat.mkv", movie("Heat", Some(1995)), None),
        upsert("b/Alien.mkv", movie("Alien", Some(1979)), None),
    ]);
    let heat = e.find_top("Heat");
    let fid = e.file_id("a/Heat.mkv");
    // `mv a/Heat.mkv b/Alien.mkv` overwrote Alien.
    let mut to = facts("b/Alien.mkv");
    to.fp128 = None;
    let out = e.batch(ScanBatch {
        root_id: e.root,
        moves: vec![FileMove {
            file_id: fid,
            to,
            guess: Some(movie("Heat", Some(1995))),
        }],
        ..Default::default()
    });
    assert_eq!(out.files_moved, 1);
    let snap = e
        .cat
        .read_blocking(|c| scan_support::load_root_snapshot(c, e.root))
        .unwrap();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap["b/Alien.mkv"].id, fid);
    assert_eq!(e.top().len(), 1, "Alien lost its only file");
    assert_eq!(e.find_top("Heat").id, heat.id);
}

#[test]
fn purge_deletes_expired_tombstones_but_never_on_unavailable_roots() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert("Lost/S01E01.mkv", episode("Lost", Some(2004), 1, &[1]), None),
        upsert("Lost/S01E02.mkv", episode("Lost", Some(2004), 1, &[2]), None),
        upsert("Dark/S01E01.mkv", episode("Dark", Some(2017), 1, &[1]), None),
    ]);
    let f1 = e.file_id("Lost/S01E01.mkv");
    let f2 = e.file_id("Lost/S01E02.mkv");
    e.batch(ScanBatch {
        root_id: e.root,
        missing: vec![f1, f2],
        ..Default::default()
    });
    let far_future = flux_catalog::now_ms() + 365 * 24 * 3600 * 1000;

    // Root offline: the tombstone clock is paused.
    e.cat
        .writer()
        .set_root_availability(e.root, false)
        .wait()
        .unwrap();
    let p = e.cat.writer().purge_tombstones(far_future).wait().unwrap();
    assert_eq!(p.files_deleted, 0);
    e.cat.writer().set_root_availability(e.root, true).wait().unwrap();

    // Not yet expired.
    let p = e
        .cat
        .writer()
        .purge_tombstones(flux_catalog::now_ms())
        .wait()
        .unwrap();
    assert_eq!(p.files_deleted, 0);

    let p = e.cat.writer().purge_tombstones(far_future).wait().unwrap();
    assert_eq!(p.files_deleted, 2);
    // 2 episodes + season + show
    assert_eq!(p.items_deleted, 4);
    let top = e.top();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].title, "Dark");
    let n: i64 = e
        .cat
        .read_blocking(|c| Ok(c.query_row("SELECT count(*) FROM item", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(n, 3);
    e.cat
        .read_blocking(|c| Ok(c.execute_batch("INSERT INTO item_fts(item_fts) VALUES('integrity-check')")?))
        .unwrap_err();
    // (query_only readers cannot run the FTS integrity-check; do it from the writer)
    e.cat
        .writer()
        .exec(|c| Ok(c.execute_batch("INSERT INTO item_fts(item_fts) VALUES('integrity-check')")?))
        .wait()
        .unwrap();
}

#[test]
fn reprobe_updates_asset_facts_and_stat_only_change_writes_little() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert(
        "Heat.mkv",
        movie("Heat", Some(1995)),
        Some(seed("h264_mkv")),
    )]);
    let heat = e.find_top("Heat");
    assert_eq!(heat.badges, vec!["720p"]);
    let snap = e
        .cat
        .read_blocking(|c| scan_support::load_root_snapshot(c, e.root))
        .unwrap();
    let row = snap["Heat.mkv"].clone();
    // The file was replaced by a 4K HDR encode at the same path.
    let mut f = facts("Heat.mkv");
    f.ino = row.ino;
    f.dev = row.dev;
    f.size = row.size + 10;
    let mut p = with_size(seed("hevc_mkv"), 3840, 2160);
    p.tracks[0].hdr = Some(flux_core::HdrInfo {
        format: flux_core::HdrFormat::Hdr10,
        mastering: None,
        cll: None,
        dovi: None,
    });
    let out = e.batch(ScanBatch {
        root_id: e.root,
        upserts: vec![FileUpsert {
            facts: f,
            probe: ProbeUpdate::Probed(Box::new(p)),
            guess: None,
        }],
        ..Default::default()
    });
    assert_eq!(out.files_changed, 1);
    assert_eq!(out.items_updated, vec![heat.id]);
    let d = e.detail(heat.id);
    assert_eq!(d.summary.badges, vec!["4K", "HDR10"]);
    assert_eq!(d.assets[0].label, "4K HEVC HDR10 · E-AC3 1.0");
    // Probe failure keeps the file browsable.
    let mut f = facts("Heat.mkv");
    f.size += 1;
    let out = e.batch(ScanBatch {
        root_id: e.root,
        upserts: vec![FileUpsert {
            facts: f,
            probe: ProbeUpdate::Failed("truncated EBML".into()),
            guess: None,
        }],
        ..Default::default()
    });
    assert_eq!(out.files_changed, 1);
    let snap = e
        .cat
        .read_blocking(|c| scan_support::load_root_snapshot(c, e.root))
        .unwrap();
    assert_eq!(snap["Heat.mkv"].state, FileState::ProbeFailed);
    assert_eq!(snap["Heat.mkv"].probe_version, None);
    assert_eq!(e.top().len(), 1);
}

#[test]
fn large_batches_are_split_and_the_cursor_waits_for_every_part() {
    let e = env(LibraryKind::Movies);
    let up: Vec<_> = (0..300)
        .map(|i| {
            upsert(
                &format!("f{i}.mkv"),
                movie(&format!("Film {i}"), Some(2000)),
                None,
            )
        })
        .collect();
    let before = e
        .cat
        .writer()
        .stats()
        .commands
        .load(std::sync::atomic::Ordering::Relaxed);
    let out = e.batch(ScanBatch {
        root_id: e.root,
        upserts: up,
        cursor: Some(b"evt:42".to_vec()),
        ..Default::default()
    });
    assert_eq!(out.files_added, 300);
    assert_eq!(out.items_added.len(), 300);
    let jobs = e
        .cat
        .writer()
        .stats()
        .commands
        .load(std::sync::atomic::Ordering::Relaxed)
        - before;
    assert_eq!(jobs, 3, "300 upserts → 3 jobs of ≤128");
    let root = e
        .cat
        .read_blocking(|c| scan_support::root_info(c, e.root))
        .unwrap()
        .unwrap();
    assert_eq!(root.cursor.as_deref(), Some(&b"evt:42"[..]));

    // A batch whose middle part fails reports BatchFailed, keeps the old
    // cursor, and leaves the other parts applied (each part is atomic).
    // Inject the failure with a temp trigger on the writer's connection.
    e.cat
        .writer()
        .exec(|c| {
            Ok(c.execute_batch(
                "CREATE TEMP TRIGGER fail_g150 BEFORE INSERT ON file WHEN new.rel_path = 'g150.mkv'
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )?)
        })
        .wait()
        .unwrap();
    let up: Vec<_> = (0..300)
        .map(|i| {
            upsert(
                &format!("g{i}.mkv"),
                movie(&format!("Gilm {i}"), Some(2001)),
                None,
            )
        })
        .collect();
    let err = e
        .cat
        .writer()
        .apply(ScanBatch {
            root_id: e.root,
            upserts: up,
            cursor: Some(b"evt:43".to_vec()),
            ..Default::default()
        })
        .wait()
        .unwrap_err();
    assert!(
        matches!(&err, flux_catalog::CatalogError::BatchFailed(m) if m.contains("injected")),
        "{err:?}"
    );
    let root = e
        .cat
        .read_blocking(|c| scan_support::root_info(c, e.root))
        .unwrap()
        .unwrap();
    assert_eq!(
        root.cursor.as_deref(),
        Some(&b"evt:42"[..]),
        "cursor not advanced past a failed part"
    );
    let snap = e
        .cat
        .read_blocking(|c| scan_support::load_root_snapshot(c, e.root))
        .unwrap();
    assert!(
        snap.contains_key("g0.mkv") && snap.contains_key("g299.mkv"),
        "parts 1 and 3 committed"
    );
    assert!(
        !snap.contains_key("g150.mkv") && !snap.contains_key("g200.mkv"),
        "part 2 rolled back"
    );
}

#[test]
fn a_corrupt_stored_guess_does_not_poison_later_batches() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert(
        "x/Featurettes/a.mkv",
        extra(None, None, flux_core::ExtraKind::Featurette),
        None,
    )]);
    e.cat
        .writer()
        .exec(|c| {
            Ok(c.execute(
                "UPDATE file_meta SET guess = '{not json'
                 WHERE file_id = (SELECT id FROM file WHERE rel_path = 'x/Featurettes/a.mkv')",
                [],
            )?)
        })
        .wait()
        .unwrap();
    // A later batch places a movie, which retries pending extras.
    let out = e.apply(vec![upsert("y/Heat.mkv", movie("Heat", Some(1995)), None)]);
    assert_eq!(out.files_added, 1);
    // And re-sending the corrupt file without a guess treats it as unknown.
    let out = e.batch(ScanBatch {
        root_id: e.root,
        upserts: vec![FileUpsert {
            facts: facts("x/Featurettes/a.mkv"),
            probe: ProbeUpdate::Keep,
            guess: None,
        }],
        ..Default::default()
    });
    assert_eq!(out.files_changed, 1);
}
