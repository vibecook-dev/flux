//! Per-user state: progress, watched, continue watching with "next up",
//! unwatched counts, and per-user list filters.

mod common;

use common::*;
use flux_catalog::{read, ItemsQuery};
use flux_core::api::{Id, Identity, ItemFilter, ItemKind, ItemSummary, Role};
use flux_core::LibraryKind;

fn user(e: &Env, login: &str) -> Id {
    e.cat
        .writer()
        .ensure_user(Identity {
            login_name: login.into(),
            display_name: login.into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Viewer,
            tags: Vec::new(),
        })
        .wait()
        .unwrap()
}

fn cw(e: &Env, u: Id) -> Vec<ItemSummary> {
    e.cat
        .read_blocking(move |c| read::continue_watching(c, u, 20))
        .unwrap()
}

fn codes(v: &[ItemSummary]) -> Vec<String> {
    v.iter()
        .map(|i| match &i.subtitle {
            Some(s) => format!("{} {s}", i.parent_title.clone().unwrap_or_default()),
            None => i.title.clone(),
        })
        .collect()
}

/// Two seasons of 3 episodes plus a special, and two movies.
fn library(e: &Env) -> (Id, Vec<Vec<Id>>) {
    let mut v = Vec::new();
    for s in 1..=2u16 {
        for n in 1..=3u16 {
            v.push(upsert(
                &format!("Lost/Season {s}/Lost - S{s:02}E{n:02}.mkv"),
                episode("Lost", None, s, &[n]),
                None,
            ));
        }
    }
    v.push(upsert(
        "Lost/Specials/Lost - S00E01.mkv",
        episode("Lost", None, 0, &[1]),
        None,
    ));
    v.push(upsert("Heat (1995).mkv", movie("Heat", Some(1995)), None));
    v.push(upsert("Alien (1979).mkv", movie("Alien", Some(1979)), None));
    e.apply(v);
    let show = e.find_top("Lost").id;
    let seasons: Vec<Vec<Id>> = e
        .children(show)
        .into_iter()
        .map(|s| e.children(s.id).into_iter().map(|x| x.id).collect())
        .collect();
    (show, seasons) // seasons[0] = S1, [1] = S2, [2] = Specials
}

const H: u64 = 2_400_000;

#[test]
fn next_up_follows_finished_episodes_across_seasons() {
    let e = env(LibraryKind::Mixed);
    let (show, s) = library(&e);
    let u = user(&e, "a@x");
    let w = e.cat.writer();
    assert!(cw(&e, u).is_empty());

    // Finish S1E3 (≥ 90 %): S1E4 doesn't exist, so S2E1 is next up.
    w.set_progress(u, s[0][2], None, H * 95 / 100, H).wait().unwrap();
    assert_eq!(codes(&cw(&e, u)), ["Lost S02E01"]);
    // Finish S1E1 later: the next unfinished after S1E1 is S1E2.
    std::thread::sleep(std::time::Duration::from_millis(2));
    w.set_progress(u, s[0][0], None, H, H).wait().unwrap();
    assert_eq!(codes(&cw(&e, u)), ["Lost S01E02"]);
    // Start a movie: most recent first; one entry per show.
    std::thread::sleep(std::time::Duration::from_millis(2));
    let heat = e.find_top("Heat").id;
    w.set_progress(u, heat, None, 60_000, 6_000_000).wait().unwrap();
    assert_eq!(codes(&cw(&e, u)), ["Heat", "Lost S01E02"]);
    // Pause half-way through S1E2: it shows with its resume point.
    std::thread::sleep(std::time::Duration::from_millis(2));
    w.set_progress(u, s[0][1], None, H / 2, H).wait().unwrap();
    let v = cw(&e, u);
    assert_eq!(codes(&v), ["Lost S01E02", "Heat"]);
    assert_eq!(v[0].progress.as_ref().unwrap().position_ms, H / 2);
    // Finish the movie: it drops out.
    w.set_progress(u, heat, None, 5_900_000, 6_000_000)
        .wait()
        .unwrap();
    assert_eq!(codes(&cw(&e, u)), ["Lost S01E02"]);
    // Specials never come up next after a regular episode.
    for &ep in s[0].iter().chain(&s[1]) {
        w.set_watched(u, ep, true).wait().unwrap();
    }
    assert!(cw(&e, u).is_empty(), "show finished: {:?}", codes(&cw(&e, u)));
    // Another user sees none of this.
    let b = user(&e, "b@x");
    assert!(cw(&e, b).is_empty());
    // Home uses the same list.
    let home = e.cat.read_blocking(move |c| read::home(c, Some(u), 10)).unwrap();
    assert!(home.continue_watching.is_empty());
    let _ = show;
}

#[test]
fn a_double_episode_file_finishes_both_episodes() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert(
            "Saul/Season 1/Saul - S01E01E02.mkv",
            episode("Saul", None, 1, &[1, 2]),
            None,
        ),
        upsert(
            "Saul/Season 1/Saul - S01E03.mkv",
            episode("Saul", None, 1, &[3]),
            None,
        ),
    ]);
    let show = e.find_top("Saul").id;
    let season = e.children(show)[0].id;
    let eps: Vec<Id> = e.children(season).iter().map(|x| x.id).collect();
    let u = user(&e, "a@x");
    e.cat.writer().set_progress(u, eps[0], None, H, H).wait().unwrap();
    let p2 = e
        .cat
        .read_blocking(move |c| read::progress(c, u, eps[1]))
        .unwrap()
        .unwrap();
    assert!(p2.watched, "E02 is in the same file");
    assert_eq!(codes(&cw(&e, u)), ["Saul S01E03"]);
}

#[test]
fn unwatched_counts_and_filters_are_per_user() {
    let e = env(LibraryKind::Mixed);
    let (show, s) = library(&e);
    let (a, b) = (user(&e, "a@x"), user(&e, "b@x"));
    let w = e.cat.writer();
    // a: watched all of season 1; b: S2E1 in progress.
    w.set_watched(a, s[0][0], true).wait().unwrap();
    w.set_watched(a, s[0][1], true).wait().unwrap();
    w.set_watched(a, s[0][2], true).wait().unwrap();
    w.set_progress(b, s[1][0], None, 1_000, H).wait().unwrap();
    let summary = |u: Option<Id>, id: Id| {
        e.cat
            .read_blocking(move |c| read::item_summary(c, id, u))
            .unwrap()
            .unwrap()
    };
    // 7 episodes including the special.
    assert_eq!(summary(Some(a), show).unwrap_count(), 4);
    assert_eq!(summary(Some(b), show).unwrap_count(), 7);
    assert_eq!(summary(None, show).unwrap_count(), 7);
    let s1 = e.children(show)[0].id;
    assert_eq!(summary(Some(a), s1).unwrap_count(), 0);
    assert_eq!(summary(Some(b), s1).unwrap_count(), 3);

    let titles = |u: Option<Id>, f: ItemFilter, kinds: Vec<ItemKind>| {
        let lib = e.lib.id;
        let p = e
            .cat
            .read_blocking(move |c| {
                read::items_page(
                    c,
                    &ItemsQuery {
                        library_id: Some(lib),
                        kinds,
                        user_id: u,
                        filter: Some(f),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        assert_eq!(p.total, Some(p.items.len() as u64));
        p.items.into_iter().map(|i| i.title).collect::<Vec<_>>()
    };
    assert_eq!(titles(Some(b), ItemFilter::InProgress, vec![]), ["Lost"]);
    assert!(titles(Some(a), ItemFilter::InProgress, vec![]).is_empty());
    assert_eq!(
        titles(Some(a), ItemFilter::Unwatched, vec![]),
        ["Alien", "Heat", "Lost"]
    );
    assert_eq!(
        titles(Some(a), ItemFilter::Unwatched, vec![ItemKind::Season]),
        ["Season 2", "Specials"],
        "season 1 is done for a"
    );
    assert_eq!(
        titles(Some(b), ItemFilter::InProgress, vec![ItemKind::Season]),
        ["Season 2"]
    );
    assert_eq!(
        titles(Some(b), ItemFilter::InProgress, vec![ItemKind::Episode]),
        ["Episode 1"]
    );
    // a finishes the rest: Lost no longer unwatched for a, still for b.
    for &ep in s[1].iter().chain(&s[2]) {
        w.set_watched(a, ep, true).wait().unwrap();
    }
    assert_eq!(titles(Some(a), ItemFilter::Unwatched, vec![]), ["Alien", "Heat"]);
    assert_eq!(
        titles(Some(b), ItemFilter::Unwatched, vec![]),
        ["Alien", "Heat", "Lost"]
    );
}

trait Count {
    fn unwrap_count(&self) -> u32;
}
impl Count for ItemSummary {
    fn unwrap_count(&self) -> u32 {
        self.unwatched_count
            .expect("shows and seasons carry unwatched_count")
    }
}

#[test]
fn show_badges_merge_from_episodes_for_the_4k_and_hdr_filters() {
    let e = env(LibraryKind::Mixed);
    e.apply(vec![
        upsert(
            "Dune Prophecy/Season 1/x - S01E01.mkv",
            episode("Dune Prophecy", None, 1, &[1]),
            Some(with_size(seed("hevc_mkv"), 3840, 2160)),
        ),
        upsert(
            "Dune Prophecy/Season 1/x - S01E02.mkv",
            episode("Dune Prophecy", None, 1, &[2]),
            Some(seed("h264_mkv")),
        ),
        upsert(
            "Lost/Season 1/x - S01E01.mkv",
            episode("Lost", None, 1, &[1]),
            Some(seed("h264_mkv")),
        ),
    ]);
    let dune = e.find_top("Dune Prophecy");
    assert!(dune.badges.contains(&"4K".to_string()), "{:?}", dune.badges);
    let lib = e.lib.id;
    let p = e
        .cat
        .read_blocking(move |c| {
            read::items_page(
                c,
                &ItemsQuery {
                    library_id: Some(lib),
                    filter: Some(ItemFilter::Uhd),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    assert_eq!(
        p.items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
        ["Dune Prophecy"]
    );
}
