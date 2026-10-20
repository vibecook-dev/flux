//! Grouping rules end to end: files → assets → items, read back via the API queries.

mod common;

use common::*;
use flux_core::api::ItemKind;
use flux_core::{ExtraKind, LibraryKind};

#[test]
fn versions_and_editions_are_assets_of_one_movie() {
    let e = env(LibraryKind::Movies);
    let mut cut = movie("Blade Runner", Some(1982));
    cut.edition = Some("Final Cut".into());
    let out = e.apply(vec![
        upsert(
            "Blade Runner (1982)/Blade Runner (1982) - 2160p.mkv",
            movie("Blade Runner", Some(1982)),
            Some(with_size(seed("hevc_mkv"), 3840, 2160)),
        ),
        upsert(
            "Blade Runner (1982)/Blade Runner (1982) - 1080p.mkv",
            movie("Blade Runner", Some(1982)),
            Some(with_size(seed("h264_mkv"), 1920, 1080)),
        ),
        upsert(
            "Blade Runner (1982)/Blade Runner (1982) {edition-Final Cut}.mkv",
            cut,
            Some(seed("h264_mkv")),
        ),
        // Punctuation/case differences still group.
        upsert(
            "blade.runner.1982.720p.mkv",
            movie("blade runner", Some(1982)),
            Some(seed("h264_mkv")),
        ),
    ]);
    assert_eq!(out.files_added, 4);
    let top = e.top();
    assert_eq!(top.len(), 1, "{top:#?}");
    let d = e.detail(top[0].id);
    assert_eq!(d.assets.len(), 4);
    // Editionless versions first, best first; then editions.
    assert_eq!(d.assets[0].label, "4K HEVC · E-AC3 1.0");
    assert_eq!(d.assets.last().unwrap().edition.as_deref(), Some("Final Cut"));
    assert_eq!(d.summary.badges, vec!["4K"]);
    assert!(d.assets.iter().all(|a| a.available && a.probe.is_some()));
    assert!(d.assets[0].path.starts_with("/media/lib/Blade Runner (1982)/"));
}

#[test]
fn remakes_stay_separate_and_year_rules_merge_carefully() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("Dune (1984).mkv", movie("Dune", Some(1984)), None),
        upsert("Dune (2021).mkv", movie("Dune", Some(2021)), None),
        // No year, two candidates → ambiguous → its own item.
        upsert("Dune.mkv", movie("Dune", None), None),
        // No year, one candidate → joins it.
        upsert("Heat (1995).mkv", movie("Heat", Some(1995)), None),
        upsert("heat.mkv", movie("Heat", None), None),
    ]);
    let top = e.top();
    let dunes: Vec<_> = top.iter().filter(|i| i.title == "Dune").collect();
    assert_eq!(dunes.len(), 3);
    let heat = e.find_top("Heat");
    assert_eq!(e.detail(heat.id).assets.len(), 2);
    // A yearless item adopts the year when a dated file arrives.
    e.apply(vec![upsert("Arrival.mkv", movie("Arrival", None), None)]);
    e.apply(vec![upsert(
        "Arrival (2016).mkv",
        movie("Arrival", Some(2016)),
        None,
    )]);
    let arrival = e.find_top("Arrival");
    assert_eq!(arrival.year, Some(2016));
    assert_eq!(e.detail(arrival.id).assets.len(), 2);
}

#[test]
fn explicit_tmdb_id_wins_over_title() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert(
            "Amélie (2001) [tmdbid-194]/a.mp4",
            movie_tmdb("Amélie", Some(2001), 194),
            None,
        ),
        // Different spelling, same id → same item.
        upsert(
            "Le Fabuleux Destin [tmdbid-194].mkv",
            movie_tmdb("Le Fabuleux Destin d'Amélie Poulain", Some(2001), 194),
            None,
        ),
        // Same title/year but a different id → different film.
        upsert(
            "Amelie (2001) [tmdbid-999].mkv",
            movie_tmdb("Amelie", Some(2001), 999),
            None,
        ),
        // No id: joins the first compatible same-title item and fills nothing in.
        upsert("Amelie.2001.mkv", movie("Amelie", Some(2001)), None),
    ]);
    let top = e.top();
    assert_eq!(top.len(), 2, "{top:#?}");
    let a = top
        .iter()
        .find(|i| {
            e.detail(i.id)
                .external_ids
                .contains(&("tmdb".into(), "194".into()))
        })
        .unwrap();
    assert_eq!(e.detail(a.id).assets.len(), 3);
}

#[test]
fn multi_part_assets_are_ordered_by_part_and_runtime_sums() {
    let e = env(LibraryKind::Movies);
    let mut p2 = movie("Spirited Away", Some(2001));
    p2.part = Some(2);
    let mut p1 = movie("Spirited Away", Some(2001));
    p1.part = Some(1);
    e.apply(vec![
        upsert(
            "Spirited Away (2001)/Spirited Away (2001) - cd2.mkv",
            p2,
            Some(seed("h264_mkv")),
        ),
        upsert(
            "Spirited Away (2001)/Spirited Away (2001) - cd1.mkv",
            p1,
            Some(seed("h264_mkv")),
        ),
    ]);
    let d = e.detail(e.find_top("Spirited Away").id);
    assert_eq!(d.assets.len(), 2);
    assert!(d.assets[0].path.ends_with("cd1.mkv"));
    let one = d.assets[0].duration_ms.unwrap();
    assert_eq!(d.runtime_ms, Some(one * 2));
}

#[test]
fn episodes_group_into_show_season_episode_and_multi_episode_files_link_twice() {
    let e = env(LibraryKind::Shows);
    let out = e.apply(vec![
        upsert(
            "The Wire (2002)/Season 01/The Wire - S01E01.mkv",
            episode("The Wire", Some(2002), 1, &[1]),
            Some(seed("h264_mkv")),
        ),
        upsert(
            "The Wire (2002)/Season 01/The Wire - S01E02E03.mkv",
            episode("The Wire", Some(2002), 1, &[2, 3]),
            Some(seed("h264_mkv")),
        ),
        // A yearless name in the same show folder joins the dated show.
        upsert(
            "The Wire (2002)/Season 02/The.Wire.S02E01.mkv",
            episode("The Wire", None, 2, &[1]),
            None,
        ),
        upsert(
            "The Wire (2002)/Specials/The Wire - S00E01.mkv",
            episode("The Wire", Some(2002), 0, &[1]),
            None,
        ),
    ]);
    // show + 3 seasons + 5 episodes
    assert_eq!(out.items_added.len(), 9, "{out:?}");
    let top = e.top();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].kind, ItemKind::Show);
    assert_eq!(top[0].child_count, Some(3));
    let seasons = e.children(top[0].id);
    let names: Vec<_> = seasons.iter().map(|s| s.subtitle.clone().unwrap()).collect();
    assert_eq!(
        names,
        vec!["Season 1", "Season 2", "Specials"],
        "Specials sort last"
    );
    assert!(seasons
        .iter()
        .all(|s| s.parent_title.as_deref() == Some("The Wire")));
    let titles: Vec<_> = seasons.iter().map(|s| s.title.clone()).collect();
    assert_eq!(titles, vec!["Season 1", "Season 2", "Specials"]);
    let eps = e.children(seasons[0].id);
    let codes: Vec<_> = eps.iter().map(|s| s.subtitle.clone().unwrap()).collect();
    assert_eq!(codes, vec!["S01E01", "S01E02", "S01E03"]);
    // E02 and E03 share one asset.
    let d2 = e.detail(eps[1].id);
    let d3 = e.detail(eps[2].id);
    assert_eq!(d2.assets.len(), 1);
    assert_eq!(d2.assets[0].id, d3.assets[0].id);
    assert_eq!(d3.episode_numbers, vec![2, 3]);
    assert_eq!(d3.show_id, Some(top[0].id));
    assert_eq!(d3.season_number, Some(1));
}

#[test]
fn extras_attach_by_folder_even_when_scanned_first_and_samples_are_ignored() {
    let e = env(LibraryKind::Movies);
    // The extra arrives in an earlier batch than its movie.
    let out = e.apply(vec![upsert(
        "Mad Max - Fury Road (2015)/Featurettes/Making Of.mp4",
        extra(None, None, ExtraKind::Featurette),
        Some(seed("h264_mp4")),
    )]);
    assert_eq!(out.files_pending, 1);
    assert!(e.top().is_empty());
    let out = e.apply(vec![
        upsert(
            "Mad Max - Fury Road (2015)/Mad Max - Fury Road (2015).mkv",
            movie("Mad Max: Fury Road", Some(2015)),
            Some(seed("hevc_mkv")),
        ),
        upsert(
            "Mad Max - Fury Road (2015)/Sample/mad-max-sample.mkv",
            sample(),
            Some(seed("hevc_mkv")),
        ),
        // A trailer next to the movie file, same batch.
        upsert(
            "Mad Max - Fury Road (2015)/Mad Max - Fury Road (2015)-trailer.mp4",
            extra(Some("Mad Max: Fury Road"), Some(2015), ExtraKind::Trailer),
            None,
        ),
    ]);
    assert_eq!(out.files_ignored, 1);
    let top = e.top();
    assert_eq!(top.len(), 1, "sample and extras are not top-level: {top:#?}");
    let kids = e.children(top[0].id);
    assert_eq!(kids.len(), 2, "{kids:#?}");
    assert!(kids.iter().all(|k| k.kind == ItemKind::Extra));
    let titles: Vec<_> = kids.iter().map(|k| k.title.as_str()).collect();
    assert!(titles.contains(&"Making Of"));
    // The movie's own detail lists only its own asset.
    assert_eq!(e.detail(top[0].id).assets.len(), 1);
}

#[test]
fn show_extras_attach_to_the_show() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert(
            "Dark (2017)/Season 01/Dark - S01E01.mkv",
            episode("Dark", Some(2017), 1, &[1]),
            None,
        ),
        upsert(
            "Dark (2017)/Extras/Inside Dark.mkv",
            extra(None, None, ExtraKind::BehindTheScenes),
            None,
        ),
    ]);
    let show = e.find_top("Dark");
    let kids = e.children(show.id);
    assert_eq!(kids.len(), 2);
    assert_eq!(kids[0].kind, ItemKind::Season);
    assert_eq!(kids[1].kind, ItemKind::Extra);
    assert_eq!(show.child_count, Some(1), "extras do not count as seasons");
}

#[test]
fn unknown_files_become_unmatched_movies_named_after_the_file() {
    let e = env(LibraryKind::Mixed);
    let out = e.apply(vec![
        upsert(
            "Home Videos/beach_trip.[2019].mp4",
            flux_core::NameGuess::unknown(),
            None,
        ),
        flux_catalog::FileUpsert {
            guess: None,
            ..upsert("random/clip.mkv", flux_core::NameGuess::unknown(), None)
        },
    ]);
    assert_eq!(out.files_added, 2);
    let titles: Vec<_> = e.top().into_iter().map(|i| i.title).collect();
    assert!(titles.contains(&"beach trip".to_string()), "{titles:?}");
    assert!(titles.contains(&"clip".to_string()), "{titles:?}");
    // Unmatched items are queued for the matcher with higher priority.
    let jobs = e
        .cat
        .writer()
        .claim_jobs(flux_catalog::ClaimJobs::new(
            "test",
            &["match"],
            10,
            std::time::Duration::from_secs(60),
        ))
        .wait()
        .unwrap();
    assert_eq!(jobs.len(), 2);
    assert!(jobs
        .iter()
        .all(|j| j.priority == flux_catalog::jobs::priority::UNMATCHED));
}

#[test]
fn renamed_file_regroups_and_keeps_its_asset() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert(
        "Teh Matrix.mkv",
        movie("Teh Matrix", Some(1999)),
        Some(seed("h264_mkv")),
    )]);
    let wrong = e.find_top("Teh Matrix");
    let asset = e.detail(wrong.id).assets[0].id;
    let file_id = e.file_id("Teh Matrix.mkv");
    let mut to = facts("The Matrix (1999).mkv");
    to.fp128 = None;
    let out = e.batch(flux_catalog::ScanBatch {
        root_id: e.root,
        moves: vec![flux_catalog::FileMove {
            file_id,
            to,
            guess: Some(movie("The Matrix", Some(1999))),
        }],
        ..Default::default()
    });
    assert_eq!(out.files_moved, 1);
    assert_eq!(out.items_removed, vec![wrong.id], "the misnamed item is hidden");
    let right = e.find_top("The Matrix");
    let d = e.detail(right.id);
    assert_eq!(d.assets[0].id, asset, "same asset, new item");
    assert!(d.assets[0].path.ends_with("The Matrix (1999).mkv"));
}
