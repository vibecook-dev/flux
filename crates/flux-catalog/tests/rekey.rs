//! Episode re-keying (integration §4.1 #2): anime absolute numbering mapped
//! to seasons after the fact, carrying user state; future files follow the map.

mod common;

use common::*;
use flux_catalog::{read, rekey, EpisodeRekey};
use flux_core::api::{Event, Id, Identity, ItemKind, Role};
use flux_core::{GuessKind, LibraryKind, NameGuess};

fn absolute(show: &str, abs: u32) -> NameGuess {
    let mut g = NameGuess::unknown();
    g.kind = GuessKind::Episode;
    g.title = Some(show.into());
    g.absolute_episode = Some(abs);
    g.confidence = 0.8;
    g
}

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

/// `(season number, episode number)` of every episode of a show, with ids.
fn layout(e: &Env, show: Id) -> Vec<(u16, u16, Id)> {
    let mut out = Vec::new();
    for s in e
        .children(show)
        .into_iter()
        .filter(|s| s.kind == ItemKind::Season)
    {
        for ep in e.children(s.id) {
            // "S02E01"
            let code = ep.subtitle.clone().unwrap();
            let (sn, en) = code[1..].split_once('E').unwrap();
            out.push((sn.parse().unwrap(), en.parse().unwrap(), ep.id));
        }
    }
    out
}

#[test]
fn absolute_episodes_move_to_their_seasons_with_user_state_and_the_map_sticks() {
    let e = env(LibraryKind::Shows);
    let files: Vec<_> = (1..=4)
        .map(|n| {
            upsert(
                &format!("Frieren/[Sub] Frieren - {n:03} [1080p].mkv"),
                absolute("Frieren", n),
                Some(seed("h264_mkv")),
            )
        })
        .collect();
    e.apply(files);
    let show = e.find_top("Frieren").id;
    // Filed as season 1, episodes 1-4 (the parser knows no seasons).
    assert_eq!(
        layout(&e, show).iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(),
        vec![(1, 1), (1, 2), (1, 3), (1, 4)]
    );
    // flux-meta asks which files need mapping.
    let todo = e
        .cat
        .read_blocking(move |c| rekey::absolute_episodes(c, show))
        .unwrap();
    assert_eq!(
        todo.iter().map(|t| t.absolute).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(todo[3].season, Some(1));

    // Two users: one finished abs 3, one is half-way through it; one has
    // abs 4 in progress.
    let (a, b) = (user(&e, "a@x"), user(&e, "b@x"));
    let ep = |n: u16| {
        layout(&e, show)
            .into_iter()
            .find(|x| x.0 == 1 && x.1 == n)
            .unwrap()
            .2
    };
    let w = e.cat.writer();
    w.set_progress(a, ep(3), None, 1_400_000, 1_440_000)
        .wait()
        .unwrap();
    w.set_progress(b, ep(3), None, 700_000, 1_440_000).wait().unwrap();
    w.set_progress(b, ep(4), None, 100_000, 1_440_000).wait().unwrap();

    // TMDB: season 1 has 2 episodes; absolute 3 and 4 are S02E01, S02E02.
    let out = w
        .rekey_episodes(
            todo.iter()
                .filter(|t| t.absolute >= 3)
                .map(|t| EpisodeRekey {
                    asset_id: t.asset_id,
                    season: 2,
                    episodes: vec![(t.absolute - 2) as u16],
                    absolute: Some(t.absolute),
                })
                .collect(),
        )
        .wait()
        .unwrap();
    assert_eq!(out.assets_moved, 2);
    assert_eq!(out.mappings, 2);
    assert_eq!(out.user_states_moved, 3);
    // New season 2 + two episodes added; the two old placeholders removed.
    assert_eq!(out.items_added.len(), 3, "{out:?}");
    assert_eq!(out.items_removed.len(), 2, "{out:?}");
    match out.event().unwrap() {
        Event::ItemsChanged {
            library_id,
            added,
            removed,
            ..
        } => {
            assert_eq!(library_id, e.lib.id);
            assert_eq!((added.len(), removed.len()), (3, 2));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        layout(&e, show).iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(),
        vec![(1, 1), (1, 2), (2, 1), (2, 2)]
    );
    let s2e1 = layout(&e, show)
        .into_iter()
        .find(|x| x.0 == 2 && x.1 == 1)
        .unwrap()
        .2;
    let s2e2 = layout(&e, show)
        .into_iter()
        .find(|x| x.0 == 2 && x.1 == 2)
        .unwrap()
        .2;
    let prog = |u: Id, i: Id| e.cat.read_blocking(move |c| read::progress(c, u, i)).unwrap();
    assert!(
        prog(a, s2e1).unwrap().watched,
        "a's watched state followed the file"
    );
    assert_eq!(
        prog(b, s2e1).unwrap().position_ms,
        700_000,
        "b's resume point too"
    );
    assert_eq!(prog(b, s2e2).unwrap().position_ms, 100_000);
    // The asset plays from its new episode.
    assert_eq!(e.detail(s2e1).assets.len(), 1);
    // Seasons and unwatched counts follow (a finished one episode of four).
    let summary = e
        .cat
        .read_blocking(move |c| read::item_summary(c, show, Some(a)))
        .unwrap()
        .unwrap();
    assert_eq!(summary.child_count, Some(2));
    assert_eq!(summary.unwatched_count, Some(3));

    // Re-keying again to the same place changes nothing.
    let again = w
        .rekey_episodes(vec![EpisodeRekey {
            asset_id: todo[2].asset_id,
            season: 2,
            episodes: vec![1],
            absolute: Some(3),
        }])
        .wait()
        .unwrap();
    assert_eq!(again.assets_moved, 0);
    assert!(again.event().is_none());

    // A rescan that re-parses abs 4, and a new file for abs 3 (another
    // release) land where the map says, not in season 1.
    let path4 = todo[3].rel_path.clone();
    let mut re4 = upsert(&path4, absolute("Frieren", 4), Some(seed("h264_mkv")));
    re4.facts.rel_path = path4;
    e.apply(vec![
        re4,
        upsert(
            "Frieren/[Other] Frieren - 03 (BD 1080p).mkv",
            absolute("Frieren", 3),
            Some(seed("h264_mkv")),
        ),
    ]);
    let l = layout(&e, show);
    assert_eq!(
        l.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(),
        vec![(1, 1), (1, 2), (2, 1), (2, 2)],
        "{l:?}"
    );
    assert_eq!(
        e.detail(s2e1).assets.len(),
        2,
        "the new release is a second version of S02E01"
    );
}

#[test]
fn rekey_to_a_double_episode_and_errors() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![upsert("Show/Show - 007.mkv", absolute("Show", 7), None)]);
    let show = e.find_top("Show").id;
    let asset = e
        .cat
        .read_blocking(move |c| rekey::absolute_episodes(c, show))
        .unwrap()[0]
        .asset_id;
    let out = e
        .cat
        .writer()
        .rekey_episodes(vec![EpisodeRekey {
            asset_id: asset,
            season: 1,
            episodes: vec![3, 4],
            absolute: Some(7),
        }])
        .wait()
        .unwrap();
    assert_eq!(out.assets_moved, 1);
    let l = layout(&e, show);
    assert_eq!(
        l.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(),
        vec![(1, 3), (1, 4)]
    );
    // Both episodes share the asset.
    assert_eq!(e.detail(l[0].2).assets[0].id, e.detail(l[1].2).assets[0].id);
    let w = e.cat.writer();
    assert!(w
        .rekey_episodes(vec![EpisodeRekey {
            asset_id: asset,
            season: 1,
            episodes: vec![],
            absolute: None,
        }])
        .wait()
        .is_err());
    assert!(w
        .rekey_episodes(vec![EpisodeRekey {
            asset_id: 999_999,
            season: 1,
            episodes: vec![1],
            absolute: None,
        }])
        .wait()
        .is_err());
    // A movie's asset is not an episode.
    e.apply(vec![upsert("Heat (1995).mkv", movie("Heat", Some(1995)), None)]);
    let heat = e.find_top("Heat").id;
    let movie_asset = e.detail(heat).assets[0].id;
    assert!(w
        .rekey_episodes(vec![EpisodeRekey {
            asset_id: movie_asset,
            season: 1,
            episodes: vec![1],
            absolute: None,
        }])
        .wait()
        .is_err());
}
