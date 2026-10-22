//! Pagination, search, home, progress.

mod common;

use std::collections::HashSet;

use common::*;
use flux_catalog::{read, ItemSort, ItemsQuery};
use flux_core::api::{Identity, ItemKind, Role};
use flux_core::LibraryKind;

fn titles(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("Film {i}")).collect()
}

const ALL_SORTS: [ItemSort; 6] = [
    ItemSort::Title,
    ItemSort::TitleDesc,
    ItemSort::Year,
    ItemSort::YearDesc,
    ItemSort::Added,
    ItemSort::AddedDesc,
];

/// Every page of a query, following cursors; asserts `total` on each page.
fn all_pages(e: &Env, q: ItemsQuery, expect_total: u64) -> Vec<flux_core::api::ItemSummary> {
    let mut seen = Vec::new();
    let mut q = q;
    loop {
        let page = e.cat.read_blocking(|c| read::items_page(c, &q)).unwrap();
        assert_eq!(page.total, Some(expect_total), "total on every page");
        seen.extend(page.items);
        match page.next {
            Some(n) => q.cursor = Some(n),
            None => break,
        }
    }
    seen
}

#[test]
fn keyset_pagination_for_every_sort_is_complete_ordered_and_stable() {
    let e = env(LibraryKind::Movies);
    let ts = titles(250);
    // Several batches so added_at differs between groups; repeated years
    // and unknown years exercise the tie-breaks.
    for chunk in ts.chunks(100) {
        e.apply(
            chunk
                .iter()
                .map(|t| {
                    let n: u16 = t[5..].parse().unwrap();
                    let year = (!n.is_multiple_of(11)).then_some(1990 + n % 7);
                    upsert(&format!("{t}.mkv"), movie(t, year), None)
                })
                .collect(),
        );
        std::thread::sleep(std::time::Duration::from_millis(3));
    }
    let sort_key = |s: &str| flux_catalog::group::sort_title(s);
    for sort in ALL_SORTS {
        let seen = all_pages(
            &e,
            ItemsQuery {
                library_id: Some(e.lib.id),
                sort,
                limit: 37,
                ..Default::default()
            },
            250,
        );
        assert_eq!(seen.len(), 250, "{sort:?}");
        assert_eq!(
            seen.iter().map(|s| s.id).collect::<HashSet<_>>().len(),
            250,
            "{sort:?}: no duplicates"
        );
        let mut expect = seen.clone();
        match sort {
            ItemSort::Title => expect.sort_by_key(|i| (sort_key(&i.title), i.id)),
            ItemSort::TitleDesc => expect.sort_by_key(|i| std::cmp::Reverse((sort_key(&i.title), i.id))),
            ItemSort::Year => {
                expect.sort_by_key(|i| (i.year.map_or(99999, i64::from), sort_key(&i.title), i.id))
            }
            ItemSort::YearDesc => expect.sort_by_key(|i| {
                (
                    std::cmp::Reverse(i.year.map_or(-1, i64::from)),
                    sort_key(&i.title),
                    i.id,
                )
            }),
            ItemSort::Added => expect.sort_by_key(|i| (i.added_at, i.id)),
            ItemSort::AddedDesc => expect.sort_by_key(|i| std::cmp::Reverse((i.added_at, i.id))),
        }
        let ids = |v: &[flux_core::api::ItemSummary]| v.iter().map(|i| i.id).collect::<Vec<_>>();
        assert_eq!(ids(&seen), ids(&expect), "{sort:?} order");
        if sort == ItemSort::Title {
            // Natural number order: Film 2 before Film 10.
            assert_eq!(
                seen.iter().map(|s| s.title.clone()).collect::<Vec<_>>(),
                titles(250)
            );
        }
        if sort == ItemSort::YearDesc {
            assert!(seen.last().unwrap().year.is_none(), "unknown years last");
            assert_eq!(seen[0].year, Some(1996));
        }
        // Every library-level sort walks an index (no temp b-tree sort).
        let plan = e
            .cat
            .read_blocking(|c| {
                read::items_page_plan(
                    c,
                    &ItemsQuery {
                        library_id: Some(e.lib.id),
                        sort,
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        assert!(
            !plan.iter().any(|p| p.contains("TEMP B-TREE")),
            "{sort:?}: {plan:?}"
        );
    }
    // A cursor from one sort is refused by another; garbage is refused.
    let page = e
        .cat
        .read_blocking(|c| {
            read::items_page(
                c,
                &ItemsQuery {
                    library_id: Some(e.lib.id),
                    sort: ItemSort::Year,
                    limit: 10,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    for (cursor, sort) in [
        (page.next.clone().unwrap(), ItemSort::Title),
        ("garbage".into(), ItemSort::Title),
    ] {
        let bad = ItemsQuery {
            library_id: Some(e.lib.id),
            sort,
            cursor: Some(cursor),
            ..Default::default()
        };
        assert!(e.cat.read_blocking(|c| read::items_page(c, &bad)).is_err());
    }
}

#[test]
fn items_page_filters_kinds_and_libraries() {
    let e = env(LibraryKind::Mixed);
    e.apply(vec![
        upsert("m/Heat (1995).mkv", movie("Heat", Some(1995)), None),
        upsert("t/Lost/S01E01.mkv", episode("Lost", Some(2004), 1, &[1]), None),
    ]);
    let q = |kinds: Vec<ItemKind>| ItemsQuery {
        library_id: Some(e.lib.id),
        kinds,
        ..Default::default()
    };
    let all = e.cat.read_blocking(|c| read::items_page(c, &q(vec![]))).unwrap();
    assert_eq!(all.items.len(), 2);
    let shows = e
        .cat
        .read_blocking(|c| read::items_page(c, &q(vec![ItemKind::Show])))
        .unwrap();
    assert_eq!(shows.items.len(), 1);
    assert_eq!(shows.items[0].title, "Lost");
    let eps = e
        .cat
        .read_blocking(|c| read::items_page(c, &q(vec![ItemKind::Episode])))
        .unwrap();
    assert_eq!(eps.items.len(), 1);
    assert_eq!(eps.items[0].subtitle.as_deref(), Some("S01E01"));
    let other = ItemsQuery {
        library_id: Some(e.lib.id + 1),
        ..Default::default()
    };
    assert!(e
        .cat
        .read_blocking(|c| read::items_page(c, &other))
        .unwrap()
        .items
        .is_empty());
    let libs = e.cat.read_blocking(read::list_libraries).unwrap();
    assert_eq!(libs.len(), 1);
    assert_eq!(libs[0].item_count, 2);
    assert_eq!(libs[0].scan.files_seen, 2);
    let roots: Vec<_> = libs[0].roots.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(roots, ["/media/lib"]);
}

/// The CJK half of a bilingual name is stored as the original title and
/// found by search; a later bilingual file fills it in on an existing item.
#[test]
fn original_titles_are_stored_and_searchable() {
    let e = env(LibraryKind::Mixed);
    let mut ep = episode("Some Show", None, 1, &[1]);
    ep.original_title = Some("示例剧".into());
    let mut film = movie("Some Film", Some(2019));
    film.original_title = Some("示例片".into());
    e.apply(vec![
        upsert("Some Show/S01/示例剧.Some.Show.S01E01.mkv", ep, None),
        upsert("示例片.Some.Film.2019.mkv", film, None),
        upsert(
            "Other Show/Other.Show.S01E01.mkv",
            episode("Other Show", None, 1, &[1]),
            None,
        ),
    ]);
    let s = |q: &str| -> Vec<String> {
        let q = q.to_string();
        e.cat
            .read_blocking(move |c| read::search(c, &q, 20, None))
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect()
    };
    assert_eq!(s("示例剧"), vec!["Some Show"]);
    assert_eq!(s("示例片"), vec!["Some Film"]);
    assert_eq!(s("some film"), vec!["Some Film"]);
    assert!(s("另一剧").is_empty());
    // A bilingual file of a show that has no original title yet fills it in.
    let mut ep2 = episode("Other Show", None, 1, &[2]);
    ep2.original_title = Some("另一剧".into());
    e.apply(vec![upsert("Other Show/另一剧.Other.Show.S01E02.mkv", ep2, None)]);
    assert_eq!(s("另一剧"), vec!["Other Show"]);
    assert_eq!(
        e.top().iter().filter(|i| i.title == "Other Show").count(),
        1,
        "still one show"
    );
    e.cat
        .writer()
        .exec(|c| Ok(c.execute_batch("INSERT INTO item_fts(item_fts) VALUES('integrity-check')")?))
        .wait()
        .unwrap();
}

/// After an upgrade the server runs a Deep scan for libraries with files
/// that an older flux-names parsed or an older flux-probe probed. Missing
/// files and failed probes don't count: a Deep scan can't bring them up to
/// date, so they would ask for one at every start.
#[test]
fn outdated_files_counts_present_files_of_older_versions() {
    let e = env(LibraryKind::Movies);
    let other = env(LibraryKind::Movies);
    let names = [
        "A (2001).mkv",
        "B (2002).mkv",
        "C (2003).mkv",
        "D (2004).mkv",
        "E (2005).mkv",
    ];
    let ups = names
        .iter()
        .map(|n| upsert(n, movie(&n[..1], None), None))
        .collect();
    e.apply(ups);
    let outdated = |e: &Env| {
        let id = e.lib.id;
        e.cat.read_blocking(move |c| read::outdated_files(c, id)).unwrap()
    };
    assert_eq!(outdated(&e), 0, "everything is current");
    e.cat
        .writer()
        .exec(|c| {
            let old_parse = flux_core::PARSER_VERSION - 1;
            let old_probe = flux_core::PROBE_VERSION - 1;
            c.execute(
                "UPDATE file SET parse_version = ?1 WHERE rel_path = 'A (2001).mkv'",
                [old_parse],
            )?;
            c.execute(
                "UPDATE file SET probe_version = ?1 WHERE rel_path = 'B (2002).mkv'",
                [old_probe],
            )?;
            c.execute(
                "UPDATE file SET parse_version = ?1, state = 'missing' WHERE rel_path = 'C (2003).mkv'",
                [old_parse],
            )?;
            c.execute(
                "UPDATE file SET parse_version = ?1, state = 'probe_failed' WHERE rel_path = 'D (2004).mkv'",
                [old_parse],
            )?;
            Ok(())
        })
        .wait()
        .unwrap();
    assert_eq!(outdated(&e), 2, "A (old parse) and B (old probe)");
    assert_eq!(outdated(&other), 0, "another library");
}

#[test]
fn search_is_prefix_diacritic_insensitive_and_ranked() {
    let e = env(LibraryKind::Mixed);
    let mut ep = episode("Breaking Bad", Some(2008), 5, &[14]);
    ep.episode_title = Some("Ozymandias".into());
    e.apply(vec![
        upsert("The Matrix (1999).mkv", movie("The Matrix", Some(1999)), None),
        upsert(
            "The Matrix Reloaded (2003).mkv",
            movie("The Matrix Reloaded", Some(2003)),
            None,
        ),
        upsert("Amélie (2001).mkv", movie("Amélie", Some(2001)), None),
        upsert(
            "Matrix of Leadership.mkv",
            movie("Leadership Matrix", Some(2010)),
            None,
        ),
        upsert("BB/S05E14.mkv", ep, None),
        upsert(
            "BB/S05E15.mkv",
            episode("Breaking Bad", Some(2008), 5, &[15]),
            None,
        ),
    ]);
    let s = |q: &str| -> Vec<String> {
        let q = q.to_string();
        e.cat
            .read_blocking(move |c| read::search(c, &q, 20, None))
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect()
    };
    let r = s("matr");
    assert_eq!(r[0], "The Matrix", "{r:?}");
    assert_eq!(r[1], "The Matrix Reloaded");
    assert!(r.contains(&"Leadership Matrix".to_string()));
    assert_eq!(s("the matrix")[0], "The Matrix");
    assert_eq!(s("amelie"), vec!["Amélie"]);
    assert_eq!(
        s("ozym"),
        vec!["Ozymandias"],
        "real episode titles are searchable"
    );
    assert!(s("episode").is_empty(), "placeholder titles are not");
    let r = s("breaking");
    assert_eq!(r, vec!["Breaking Bad"]);
    assert!(s("\" OR *").is_empty());
}

#[test]
fn progress_continue_watching_and_watched_at_ninety_percent() {
    let e = env(LibraryKind::Mixed);
    e.apply(vec![
        upsert(
            "Heat (1995).mkv",
            movie("Heat", Some(1995)),
            Some(seed("h264_mkv")),
        ),
        upsert("Lost/S01E01.mkv", episode("Lost", Some(2004), 1, &[1]), None),
        upsert("Lost/S01E02.mkv", episode("Lost", Some(2004), 1, &[2]), None),
    ]);
    let w = e.cat.writer();
    let user = w
        .ensure_user(Identity {
            login_name: "james@example.com".into(),
            display_name: "James".into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Owner,
            tags: Vec::new(),
        })
        .wait()
        .unwrap();
    let heat = e.find_top("Heat");
    let asset = e.detail(heat.id).assets[0].id;
    let p = w
        .put_progress(user, heat.id, Some(asset), 3_000, 0)
        .wait()
        .unwrap();
    // Duration falls back to the probed asset duration (seed = 6.021 s).
    assert_eq!(p.duration_ms, 6_021);
    assert!(!p.watched);
    let lost = e.find_top("Lost");
    let season = e.children(lost.id)[0].id;
    let ep1 = e.children(season)[0].id;
    w.put_progress(user, ep1, None, 600_000, 2_400_000)
        .wait()
        .unwrap();

    let home = e
        .cat
        .read_blocking(move |c| read::home(c, Some(user), 10))
        .unwrap();
    assert_eq!(home.continue_watching.len(), 2);
    // The same summary as in any list: own title, the show in parent_title.
    let ep = &home.continue_watching[0];
    assert_eq!(ep.title, "Episode 1");
    assert_eq!(ep.parent_title.as_deref(), Some("Lost"));
    assert_eq!(ep.subtitle.as_deref(), Some("S01E01"));
    assert_eq!(
        home.continue_watching[1].progress.as_ref().unwrap().position_ms,
        3_000
    );
    assert_eq!(home.recently_added.len(), 2);
    assert_eq!(home.rows.len(), 1);

    // 95 % → watched, resume point cleared, one play counted even if reported twice.
    let p = w
        .put_progress(user, heat.id, Some(asset), 5_800, 6_021)
        .wait()
        .unwrap();
    assert!(p.watched);
    assert_eq!(p.position_ms, 0);
    w.put_progress(user, heat.id, Some(asset), 5_900, 6_021)
        .wait()
        .unwrap();
    let count: i64 = e
        .cat
        .read_blocking(move |c| {
            Ok(c.query_row(
                "SELECT play_count FROM user_state WHERE item_id = ?1",
                [heat.id],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(count, 1);
    let home = e
        .cat
        .read_blocking(move |c| read::home(c, Some(user), 10))
        .unwrap();
    assert_eq!(home.continue_watching.len(), 1);
    let got = e
        .cat
        .read_blocking(move |c| read::progress(c, user, heat.id))
        .unwrap()
        .unwrap();
    assert!(got.watched);

    // Mark the whole show watched, then unwatched.
    assert_eq!(w.set_watched(user, lost.id, true).wait().unwrap(), 2);
    let eps = e
        .cat
        .read_blocking(move |c| read::children(c, season, Some(user)))
        .unwrap();
    assert!(eps.iter().all(|i| i.progress.as_ref().is_some_and(|p| p.watched)));
    assert_eq!(w.set_watched(user, lost.id, false).wait().unwrap(), 2);
    assert_eq!(
        w.set_watched(user, lost.id, false).wait().unwrap(),
        0,
        "no-op writes nothing"
    );

    // ensure_user is idempotent.
    let again = w
        .ensure_user(Identity {
            login_name: "james@example.com".into(),
            display_name: "James".into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Owner,
            tags: Vec::new(),
        })
        .wait()
        .unwrap();
    assert_eq!(again, user);
    let u = e
        .cat
        .read_blocking(|c| read::user_by_login(c, "james@example.com"))
        .unwrap()
        .unwrap();
    assert_eq!(u.role, Role::Owner);
}

#[test]
fn metadata_and_art_flow_into_summaries() {
    let e = env(LibraryKind::Mixed);
    e.apply(vec![
        upsert("Lost/S01E01.mkv", episode("Lost", Some(2004), 1, &[1]), None),
        upsert("Heat.mkv", movie("Heat", Some(1995)), None),
    ]);
    let lost = e.find_top("Lost");
    let w = e.cat.writer();
    let art = w
        .add_art(
            lost.id,
            flux_catalog::NewArt {
                kind: flux_catalog::ArtKind::Poster,
                source: flux_core::api::ArtSource::Frame,
                path: Some("/cache/lost.jpg".into()),
                url: None,
                width: Some(600),
                height: Some(900),
                blurhash: Some("LEHV6nWB2yk8".into()),
                attribution: None,
                generator: None,
                gen_version: None,
                gen_title: None,
            },
        )
        .wait()
        .unwrap();
    w.add_art(
        lost.id,
        flux_catalog::NewArt {
            kind: flux_catalog::ArtKind::Backdrop,
            source: flux_core::api::ArtSource::Provider,
            path: None,
            url: Some("https://image.tmdb.org/t/p/original/x.jpg".into()),
            width: Some(1920),
            height: Some(1080),
            blurhash: None,
            attribution: Some("TMDB".into()),
            generator: None,
            gen_version: None,
            gen_title: None,
        },
    )
    .wait()
    .unwrap();
    assert!(w
        .update_item_metadata(
            lost.id,
            flux_catalog::ItemMetadata {
                title: Some("LOST".into()),
                overview: Some("Survivors of a plane crash.".into()),
                genres: Some(vec!["Drama".into()]),
                tmdb_id: Some(4607),
                match_confidence: Some(0.97),
                match_evidence: Some(vec!["tmdb search: title+year".into()]),
                matched: true,
                ..Default::default()
            },
        )
        .wait()
        .unwrap());
    let d = e.detail(lost.id);
    assert_eq!(d.summary.title, "LOST");
    assert_eq!(d.overview.as_deref(), Some("Survivors of a plane crash."));
    assert_eq!(d.external_ids, vec![("tmdb".to_string(), "4607".to_string())]);
    assert_eq!(d.match_confidence, Some(0.97));
    assert_eq!(
        d.summary.poster.as_ref().unwrap().url,
        format!("/api/v1/art/{art}")
    );
    let poster = d.summary.poster.as_ref().unwrap();
    assert_eq!(
        (poster.attribution.as_deref(), poster.generator.as_deref()),
        (None, None)
    );
    let backdrop = d.summary.backdrop.as_ref().unwrap();
    assert_eq!(
        backdrop.attribution.as_deref(),
        Some("TMDB"),
        "provider art is credited"
    );
    // Episodes fall back to the show poster.
    let season = e.children(lost.id)[0].id;
    let ep = &e.children(season)[0];
    assert_eq!(ep.poster.as_ref().unwrap().url, format!("/api/v1/art/{art}"));
    // Search sees the new title through the FTS triggers.
    let hits = e
        .cat
        .read_blocking(|c| read::search(c, "surviv", 10, None))
        .unwrap();
    assert_eq!(hits.len(), 1);
    let row = e.cat.read_blocking(move |c| read::art(c, art)).unwrap().unwrap();
    assert_eq!(row.path.as_deref(), Some("/cache/lost.jpg"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_reads_and_writes() {
    let e = env(LibraryKind::Movies);
    let out = e
        .cat
        .writer()
        .apply(flux_catalog::ScanBatch {
            root_id: e.root,
            upserts: vec![upsert("Heat.mkv", movie("Heat", Some(1995)), None)],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(out.items_added.len(), 1);
    let lib = e.lib.id;
    let page = e
        .cat
        .read(move |c| {
            read::items_page(
                c,
                &ItemsQuery {
                    library_id: Some(lib),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    // An empty batch never reaches the writer.
    let before = e
        .cat
        .writer()
        .stats()
        .commands
        .load(std::sync::atomic::Ordering::Relaxed);
    let out = e
        .cat
        .writer()
        .apply(flux_catalog::ScanBatch {
            root_id: e.root,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(out, flux_catalog::ApplyOutcome::default());
    assert_eq!(
        e.cat
            .writer()
            .stats()
            .commands
            .load(std::sync::atomic::Ordering::Relaxed),
        before
    );
}

#[test]
fn deleting_a_library_cascades_and_rebuilds_search() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert("Heat.mkv", movie("Heat", Some(1995)), None)]);
    assert_eq!(
        e.cat
            .read_blocking(|c| read::search(c, "heat", 10, None))
            .unwrap()
            .len(),
        1
    );
    assert!(e.cat.writer().delete_library(e.lib.id).wait().unwrap());
    assert!(e
        .cat
        .read_blocking(|c| read::search(c, "heat", 10, None))
        .unwrap()
        .is_empty());
    let n: i64 = e
        .cat
        .read_blocking(|c| Ok(c.query_row("SELECT count(*) FROM file", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(n, 0);
    e.cat
        .writer()
        .exec(|c| Ok(c.execute_batch("INSERT INTO item_fts(item_fts) VALUES('integrity-check')")?))
        .wait()
        .unwrap();
}

#[test]
fn core_v2_summary_fields_root_status_and_filters() {
    use flux_core::api::ItemFilter;
    let e = env(LibraryKind::Mixed);
    e.apply(vec![
        upsert(
            "Heat (1995).mkv",
            movie("Heat", Some(1995)),
            Some(seed("h264_mkv")),
        ),
        upsert("Alien (1979).mkv", movie("Alien", Some(1979)), None),
        upsert("Lost/S01E01.mkv", episode("Lost", Some(2004), 1, &[1]), None),
        upsert("Lost/S01E02.mkv", episode("Lost", Some(2004), 1, &[2]), None),
    ]);
    let w = e.cat.writer();
    let user = w
        .ensure_user(Identity {
            login_name: "james@example.com".into(),
            display_name: "James".into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Owner,
            tags: Vec::new(),
        })
        .wait()
        .unwrap();

    // RootStatus: never scanned, reachable.
    let libs = e.cat.read_blocking(read::list_libraries).unwrap();
    assert_eq!(libs[0].roots.len(), 1);
    assert!(libs[0].roots[0].available);
    assert_eq!(libs[0].scan.files_total, Some(libs[0].scan.files_seen));
    assert_eq!(libs[0].scan.dirs_unavailable, 0);

    let lost = e.find_top("Lost");
    assert_eq!(lost.parent_title, None);
    assert!(lost.available);
    assert_eq!(lost.unwatched_count, Some(2), "anonymous: nothing watched");
    let season = e.children(lost.id)[0].clone();
    assert_eq!(
        (season.title.as_str(), season.parent_title.as_deref()),
        ("Season 1", Some("Lost"))
    );
    assert_eq!(season.subtitle.as_deref(), Some("Season 1"));
    let eps = e.children(season.id);
    assert_eq!(eps[0].parent_title.as_deref(), Some("Lost"));
    assert_eq!(eps[0].unwatched_count, None);

    // Watch E01 to the end, start Heat: per-user counts and filters.
    w.put_progress(user, eps[0].id, None, 2_390_000, 2_400_000)
        .wait()
        .unwrap();
    let heat = e.find_top("Heat");
    w.put_progress(user, heat.id, None, 3_000, 6_000_000)
        .wait()
        .unwrap();
    let lib = e.lib.id;
    let page = |filter: Option<ItemFilter>, user_id: Option<i64>| {
        e.cat
            .read_blocking(move |c| {
                read::items_page(
                    c,
                    &ItemsQuery {
                        library_id: Some(lib),
                        filter,
                        user_id,
                        ..Default::default()
                    },
                )
            })
            .unwrap()
    };
    let titles = |p: &flux_core::api::Page<flux_core::api::ItemSummary>| {
        p.items.iter().map(|i| i.title.clone()).collect::<Vec<_>>()
    };
    let all = page(None, Some(user));
    let lost_row = all.items.iter().find(|i| i.title == "Lost").unwrap();
    assert_eq!(lost_row.unwatched_count, Some(1));
    assert_eq!(
        titles(&page(Some(ItemFilter::Unwatched), Some(user))),
        ["Alien", "Heat", "Lost"]
    );
    let in_progress = page(Some(ItemFilter::InProgress), Some(user));
    assert_eq!(titles(&in_progress), ["Heat"]);
    assert_eq!(in_progress.total, Some(1), "total counts the filtered set");
    assert!(page(Some(ItemFilter::InProgress), None).items.is_empty());
    assert!(
        page(Some(ItemFilter::Uhd), Some(user)).items.is_empty(),
        "no 4K files here"
    );

    // Offline root: items stay, flagged unavailable; the root says so.
    e.cat
        .writer()
        .set_root_availability(e.root, false)
        .wait()
        .unwrap();
    let libs = e.cat.read_blocking(read::list_libraries).unwrap();
    assert!(!libs[0].roots[0].available);
    let p = page(None, Some(user));
    assert_eq!(p.items.len(), 3);
    assert!(p.items.iter().all(|i| !i.available));
}
