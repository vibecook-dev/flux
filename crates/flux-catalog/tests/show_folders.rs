//! The show-folder rule (integration §4.1 #1, the "Doctor Who" merge): a
//! different top-level show folder is a different show unless provider ids
//! agree; within one folder, yeared and yearless names still join.

mod common;

use common::*;
use flux_catalog::group::{is_season_dir, show_folder};
use flux_catalog::{FileMove, ScanBatch};
use flux_core::api::{Identity, ItemKind, Role};
use flux_core::{LibraryKind, ProviderIds};

fn shows(e: &Env) -> Vec<flux_core::api::ItemSummary> {
    e.top().into_iter().filter(|i| i.kind == ItemKind::Show).collect()
}

fn episode_count(e: &Env, show: i64) -> usize {
    e.children(show)
        .iter()
        .filter(|s| s.kind == ItemKind::Season)
        .map(|s| e.children(s.id).len())
        .sum()
}

#[test]
fn show_folder_skips_season_folders_and_packs() {
    assert_eq!(show_folder("Doctor Who/S01/x.mp4").as_deref(), Some("doctor who"));
    assert_eq!(
        show_folder("Doctor Who (1963)/Season 1/x.mkv").as_deref(),
        Some("doctor who (1963)")
    );
    assert_eq!(
        show_folder("Anime/One Piece/x - 1001.mkv").as_deref(),
        Some("anime/one piece")
    );
    assert_eq!(show_folder("Show/Specials/x.mkv").as_deref(), Some("show"));
    assert_eq!(
        show_folder("Show/Season 2/Disc 1/x.mkv").as_deref(),
        Some("show"),
        "disc folders too"
    );
    assert_eq!(
        show_folder("Daily/2024/Daily.2024-03-05.mkv").as_deref(),
        Some("daily")
    );
    assert_eq!(
        show_folder("Show.S01E01.mkv"),
        None,
        "flat files have no show folder"
    );
    assert_eq!(
        show_folder("Breaking.Bad.S01.1080p.BluRay-GRP/Breaking.Bad.S01E01.mkv"),
        None,
        "a season pack at the root is not a show folder"
    );
    for s in [
        "Season 1",
        "season.01",
        "S01",
        "Series 3",
        "Staffel 2",
        "Specials",
        "Season 1 (2005)",
        "12",
    ] {
        assert!(is_season_dir(s), "{s}");
    }
    for s in [
        "Doctor Who",
        "Dark (2017)",
        "S Club 7",
        "Se7en",
        "Season of the Witch",
        "Stargate SG-1",
        "The 4400",
    ] {
        assert!(!is_season_dir(s), "{s}");
    }
}

/// `Show/` (yearless names) and `Show (1963)/`: two shows, in either order.
#[test]
fn yearless_folder_and_dated_folder_are_two_shows_in_either_order() {
    for flip in [false, true] {
        let e = env(LibraryKind::Shows);
        let modern = vec![
            upsert(
                "Doctor Who/S01/Doctor Who - S01E01.mp4",
                episode("Doctor Who", None, 1, &[1]),
                None,
            ),
            upsert(
                "Doctor Who/S01/Doctor Who - S01E02.mp4",
                episode("Doctor Who", None, 1, &[2]),
                None,
            ),
        ];
        let classic = vec![
            upsert(
                "Doctor Who (1963)/Season 1/Doctor.Who.S01E01.mkv",
                episode("Doctor Who", Some(1963), 1, &[1]),
                None,
            ),
            upsert(
                "Doctor Who (1963)/Season 1/Doctor.Who.S01E02.mkv",
                episode("Doctor Who", Some(1963), 1, &[2]),
                None,
            ),
            upsert(
                "Doctor Who (1963)/Season 1/Doctor.Who.S01E03.mkv",
                episode("Doctor Who", Some(1963), 1, &[3]),
                None,
            ),
        ];
        if flip {
            e.apply(classic);
            e.apply(modern);
        } else {
            // Same batch, modern first.
            e.apply(modern.into_iter().chain(classic).collect());
        }
        let s = shows(&e);
        assert_eq!(s.len(), 2, "flip={flip}: {s:#?}");
        let classic = s.iter().find(|x| x.year == Some(1963)).expect("the 1963 show");
        let modern = s.iter().find(|x| x.year.is_none()).expect("the yearless show");
        assert_eq!(episode_count(&e, classic.id), 3);
        assert_eq!(episode_count(&e, modern.id), 2);
    }
}

/// `Show (2005)/` and `Show (1963)/`: two shows.
#[test]
fn two_dated_folders_are_two_shows() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert(
            "Doctor Who (2005)/Season 1/Doctor Who - S01E01.mkv",
            episode("Doctor Who", Some(2005), 1, &[1]),
            None,
        ),
        upsert(
            "Doctor Who (1963)/Season 1/Doctor Who - S01E01.mkv",
            episode("Doctor Who", Some(1963), 1, &[1]),
            None,
        ),
        // A yearless name inside the 2005 folder belongs to that folder's show.
        upsert(
            "Doctor Who (2005)/Season 1/Doctor.Who.S01E02.mkv",
            episode("Doctor Who", None, 1, &[2]),
            None,
        ),
    ]);
    let s = shows(&e);
    assert_eq!(s.len(), 2, "{s:#?}");
    let new = s.iter().find(|x| x.year == Some(2005)).unwrap();
    assert_eq!(episode_count(&e, new.id), 2);
}

/// One folder whose files carry mixed yeared and yearless guesses: one show,
/// whichever comes first (the dated guess adopts or is joined).
#[test]
fn one_folder_with_mixed_yeared_and_yearless_guesses_is_one_show() {
    for dated_first in [true, false] {
        let e = env(LibraryKind::Shows);
        let dated = upsert(
            "The Wire/Season 01/The Wire (2002) - S01E01.mkv",
            episode("The Wire", Some(2002), 1, &[1]),
            None,
        );
        let plain = upsert(
            "The Wire/Season 02/The.Wire.S02E01.mkv",
            episode("The Wire", None, 2, &[1]),
            None,
        );
        if dated_first {
            e.apply(vec![dated]);
            e.apply(vec![plain]);
        } else {
            e.apply(vec![plain]);
            e.apply(vec![dated]);
        }
        let s = shows(&e);
        assert_eq!(s.len(), 1, "dated_first={dated_first}: {s:#?}");
        assert_eq!(s[0].year, Some(2002), "the year is adopted");
        assert_eq!(episode_count(&e, s[0].id), 2);
    }
}

/// The same show folder name on two roots (two disks) is one show.
#[test]
fn same_folder_on_two_roots_is_one_show() {
    let e = env(LibraryKind::Shows);
    let root2 = e
        .cat
        .writer()
        .add_root(e.lib.id, "/media/disk2/".into(), None)
        .wait()
        .unwrap();
    e.apply(vec![upsert(
        "Lost/Season 1/Lost - S01E01.mkv",
        episode("Lost", None, 1, &[1]),
        None,
    )]);
    e.batch(ScanBatch {
        root_id: root2,
        upserts: vec![upsert(
            "Lost/Season 2/Lost - S02E01.mkv",
            episode("Lost", None, 2, &[1]),
            None,
        )],
        ..Default::default()
    });
    let s = shows(&e);
    assert_eq!(s.len(), 1, "{s:#?}");
    assert_eq!(episode_count(&e, s[0].id), 2);
}

/// Different folders whose names carry the same provider id are one show.
#[test]
fn different_folders_join_when_provider_ids_agree() {
    let e = env(LibraryKind::Shows);
    let with_id = |mut g: flux_core::NameGuess| {
        g.ids = ProviderIds {
            tmdb: None,
            tvdb: Some(78804),
            imdb: None,
        };
        g
    };
    e.apply(vec![
        upsert(
            "Doctor Who (2005) [tvdbid-78804]/Season 1/x - S01E01.mkv",
            with_id(episode("Doctor Who", Some(2005), 1, &[1])),
            None,
        ),
        upsert(
            "Doctor Who [tvdbid-78804]/Season 2/x - S02E01.mkv",
            with_id(episode("Doctor Who", None, 2, &[1])),
            None,
        ),
        // A different id in a third folder: another show.
        upsert(
            "Doctor Who (1963) [tvdbid-76107]/Season 1/x - S01E01.mkv",
            {
                let mut g = episode("Doctor Who", Some(1963), 1, &[1]);
                g.ids.tvdb = Some(76107);
                g
            },
            None,
        ),
    ]);
    let s = shows(&e);
    assert_eq!(s.len(), 2, "{s:#?}");
    let modern = s.iter().find(|x| x.year == Some(2005)).unwrap();
    assert_eq!(episode_count(&e, modern.id), 2);
}

/// Renaming a show folder (the scanner reports moves) keeps the show, its
/// items and everyone's watch state.
#[test]
fn renaming_the_show_folder_keeps_the_show_and_watch_state() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert(
            "Dark/Season 1/Dark - S01E01.mkv",
            episode("Dark", None, 1, &[1]),
            None,
        ),
        upsert(
            "Dark/Season 1/Dark - S01E02.mkv",
            episode("Dark", None, 1, &[2]),
            None,
        ),
    ]);
    let show = shows(&e)[0].clone();
    let season = e.children(show.id)[0].id;
    let ep1 = e.children(season)[0].id;
    let w = e.cat.writer();
    let user = w
        .ensure_user(Identity {
            login_name: "a@example.com".into(),
            display_name: "A".into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Viewer,
            tags: Vec::new(),
        })
        .wait()
        .unwrap();
    w.set_progress(user, ep1, None, 2_390_000, 2_400_000)
        .wait()
        .unwrap();
    let moves = ["Dark - S01E01.mkv", "Dark - S01E02.mkv"]
        .iter()
        .enumerate()
        .map(|(i, name)| FileMove {
            file_id: e.file_id(&format!("Dark/Season 1/{name}")),
            to: facts(&format!("Dark (2017)/Season 1/{name}")),
            guess: Some(episode("Dark", Some(2017), 1, &[i as u16 + 1])),
        })
        .collect();
    let out = e.batch(ScanBatch {
        root_id: e.root,
        moves,
        ..Default::default()
    });
    assert_eq!(out.files_moved, 2);
    assert!(out.items_added.is_empty(), "no new show: {out:?}");
    let s = shows(&e);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].id, show.id);
    assert_eq!(s[0].year, Some(2017));
    let p = e
        .cat
        .read_blocking(move |c| flux_catalog::read::progress(c, user, ep1))
        .unwrap()
        .unwrap();
    assert!(p.watched, "watch state survives the folder rename");
    // A new file in the old folder name now belongs to the same show.
    e.apply(vec![upsert(
        "Dark/Season 2/Dark - S02E01.mkv",
        episode("Dark", None, 2, &[1]),
        None,
    )]);
    assert_eq!(shows(&e).len(), 1);
}

/// Root-level files have no show folder: the year rules alone decide.
#[test]
fn flat_files_keep_the_title_and_year_rules() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert("Lost.S01E01.mkv", episode("Lost", None, 1, &[1]), None),
        upsert("Lost.2004.S01E02.mkv", episode("Lost", Some(2004), 1, &[2]), None),
    ]);
    let s = shows(&e);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].year, Some(2004));
}

/// Two folders stay two shows until a provider match says they are one;
/// then a merge joins seasons and episodes and keeps everyone's state.
#[test]
fn shows_from_two_folders_merge_once_provider_ids_agree() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert(
            "The Wire/Season 1/The Wire - S01E01.mkv",
            episode("The Wire", None, 1, &[1]),
            None,
        ),
        upsert(
            "The Wire/Season 1/The Wire - S01E02.mkv",
            episode("The Wire", None, 1, &[2]),
            None,
        ),
        upsert(
            "The Wire (2002)/Season 1/The Wire - S01E01 - 1080p.mkv",
            episode("The Wire", Some(2002), 1, &[1]),
            None,
        ),
        upsert(
            "The Wire (2002)/Season 2/The Wire - S02E01.mkv",
            episode("The Wire", Some(2002), 2, &[1]),
            None,
        ),
    ]);
    let s = shows(&e);
    assert_eq!(s.len(), 2);
    let (dated, plain) = if s[0].year == Some(2002) {
        (s[0].clone(), s[1].clone())
    } else {
        (s[1].clone(), s[0].clone())
    };
    let w = e.cat.writer();
    let user = w
        .ensure_user(Identity {
            login_name: "a@example.com".into(),
            display_name: "A".into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Viewer,
            tags: Vec::new(),
        })
        .wait()
        .unwrap();
    let plain_s1 = e.children(plain.id)[0].id;
    let plain_e1 = e.children(plain_s1)[0].id;
    let plain_e2 = e.children(plain_s1)[1].id;
    // E02 exists only in the plain folder (it moves); E01 in both (it merges).
    w.set_progress(user, plain_e2, None, 2_390_000, 2_400_000)
        .wait()
        .unwrap();
    w.set_progress(user, plain_e1, None, 600_000, 2_400_000)
        .wait()
        .unwrap();
    // Nothing shares ids yet.
    let none = e
        .cat
        .read_blocking(move |c| flux_catalog::merge::works_sharing_ids(c, dated.id))
        .unwrap();
    assert!(none.is_empty());
    for id in [dated.id, plain.id] {
        w.update_item_metadata(
            id,
            flux_catalog::ItemMetadata {
                tmdb_id: Some(1438),
                matched: true,
                ..Default::default()
            },
        )
        .wait()
        .unwrap();
    }
    let twins = e
        .cat
        .read_blocking(move |c| flux_catalog::merge::works_sharing_ids(c, dated.id))
        .unwrap();
    assert_eq!(twins, vec![plain.id]);
    let out = w.merge_items(plain.id, dated.id).wait().unwrap();
    assert!(out.items_removed.contains(&plain.id));
    assert_eq!(
        out.user_states_moved, 1,
        "E01's resume point merges into its twin"
    );
    let s = shows(&e);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].id, dated.id);
    assert_eq!(s[0].child_count, Some(2), "seasons 1 and 2");
    assert_eq!(
        episode_count(&e, dated.id),
        3,
        "S01E01 (two versions), S01E02, S02E01"
    );
    let s1 = e.children(dated.id)[0].id;
    let eps = e.children(s1);
    assert_eq!(e.detail(eps[0].id).assets.len(), 2, "both S01E01 files");
    let p = e
        .cat
        .read_blocking({
            let id = eps[1].id;
            move |c| flux_catalog::read::progress(c, user, id)
        })
        .unwrap()
        .unwrap();
    assert!(p.watched, "the watched S01E02 came along");
    let p1 = e
        .cat
        .read_blocking({
            let id = eps[0].id;
            move |c| flux_catalog::read::progress(c, user, id)
        })
        .unwrap()
        .unwrap();
    assert_eq!(p1.position_ms, 600_000);
    // New files in either folder now land on the merged show.
    e.apply(vec![upsert(
        "The Wire/Season 3/The Wire - S03E01.mkv",
        episode("The Wire", None, 3, &[1]),
        None,
    )]);
    assert_eq!(shows(&e).len(), 1);
    // Bad merges are refused.
    assert!(w.merge_items(dated.id, dated.id).wait().is_err());
    assert!(
        w.merge_items(s1, dated.id).wait().is_err(),
        "a season is not a top-level work"
    );
}
