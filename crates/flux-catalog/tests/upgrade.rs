//! Upgrading a catalog an older flux wrote: a deep re-parse (a new
//! `PARSER_VERSION`) re-places every file by its new guess. It heals what
//! the old rules grouped differently (a version subfolder kept as a show of
//! its own), retitles what was titled from the file name, and leaves no
//! empty item behind; flux's own images made by an older art version, or
//! for another title, are found and made again. Synthetic names only.

mod common;

use common::*;
use flux_catalog::{read, ArtKind, NewArt, JOB_ART};
use flux_core::api::{ArtSource, Id, Identity, ItemKind, Role};
use flux_core::{ExtraKind, LibraryKind};

fn user(e: &Env) -> Id {
    e.cat
        .writer()
        .ensure_user(Identity {
            login_name: "a@example.com".into(),
            display_name: "A".into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Viewer,
            tags: Vec::new(),
        })
        .wait()
        .unwrap()
}

fn shows(e: &Env) -> Vec<flux_core::api::ItemSummary> {
    e.top().into_iter().filter(|i| i.kind == ItemKind::Show).collect()
}

/// The episodes of a show, through its seasons, in order.
fn episodes(e: &Env, show: Id) -> Vec<flux_core::api::ItemSummary> {
    e.children(show)
        .into_iter()
        .filter(|s| s.kind == ItemKind::Season)
        .flat_map(|s| e.children(s.id))
        .filter(|i| i.kind == ItemKind::Episode)
        .collect()
}

fn exists(e: &Env, id: Id) -> bool {
    e.cat
        .read_blocking(move |c| read::item_library(c, id))
        .unwrap()
        .is_some()
}

fn frame(kind: ArtKind, path: &str, gen_version: Option<u32>, gen_title: Option<&str>) -> NewArt {
    NewArt {
        kind,
        source: ArtSource::Frame,
        path: Some(path.into()),
        url: None,
        width: Some(600),
        height: Some(900),
        blurhash: None,
        attribution: None,
        generator: None,
        gen_version,
        gen_title: gen_title.map(str::to_string),
    }
}

fn queued_art_jobs(e: &Env, item: Id) -> usize {
    e.cat
        .read_blocking(move |c| flux_catalog::jobs::item_jobs(c, item))
        .unwrap()
        .iter()
        .filter(|j| j.kind == JOB_ART && j.state == "queued")
        .count()
}

/// Mark every job finished, so a test sees only the jobs it causes.
fn finish_all_jobs(e: &Env) {
    e.cat
        .writer()
        .exec(|c| {
            c.execute("UPDATE job SET state = 'done', finished_at = 0", [])?;
            Ok(())
        })
        .wait()
        .unwrap();
}

fn sql(e: &Env, statement: &'static str, id: Id) {
    e.cat
        .writer()
        .exec(move |c| {
            c.execute(statement, [id])?;
            Ok(())
        })
        .wait()
        .unwrap();
}

fn fts_ok(e: &Env) {
    e.cat
        .writer()
        .exec(|c| {
            c.execute_batch("INSERT INTO item_fts(item_fts) VALUES('integrity-check');")?;
            Ok(())
        })
        .wait()
        .unwrap();
}

/// An older flux kept `Show/alt versions/` as a show of its own (same
/// title, its own folder claim). The re-parse moves those files to the
/// show of the folder around them, as more versions of the same episodes;
/// the emptied show is carried into it with its users' state and images,
/// and the show's art job is queued (its poster came from the other show).
#[test]
fn reparse_merges_a_version_subfolder_kept_as_a_show_of_its_own() {
    let e = env(LibraryKind::Shows);
    let main = |n: u16| format!("Signal.Garden/Signal.Garden.S01E0{n}.1080p.mp4");
    let alt = |n: u16| format!("Signal.Garden/alt versions/Signal Garden S01E0{n}.mkv");
    e.apply(
        (1..=3)
            .map(|n| upsert(&main(n), episode("Signal Garden", None, 1, &[n]), None))
            .collect(),
    );
    // What the old rules made of the subfolder: a second show. (Made here
    // under another title, then renamed, as today's rules would merge it.)
    e.apply(
        (1..=3)
            .map(|n| upsert(&alt(n), episode("Signal Garden Alt", None, 1, &[n]), None))
            .collect(),
    );
    let first = e.find_top("Signal Garden").id;
    let second = e.find_top("Signal Garden Alt").id;
    sql(
        &e,
        "UPDATE item SET title = 'Signal Garden', sort_title = 'signal garden', group_key = 'signal garden'
          WHERE id = ?1",
        second,
    );
    e.cat.writer().rebuild_search_index().wait().unwrap();
    assert_eq!(shows(&e).len(), 2);
    let u = user(&e);
    let alt_e2 = episodes(&e, second)[1].id;
    e.cat
        .writer()
        .set_progress(u, alt_e2, None, 120_000, 1_500_000)
        .wait()
        .unwrap();
    let old_poster = e
        .cat
        .writer()
        .add_art(
            second,
            frame(ArtKind::Poster, "art/9-poster-0000000a.jpg", None, None),
        )
        .wait()
        .unwrap();
    finish_all_jobs(&e);

    // The deep re-parse sends every file again with its (new) guess.
    let out = e.apply(
        (1..=3)
            .map(|n| upsert(&alt(n), episode("Signal Garden", None, 1, &[n]), None))
            .collect(),
    );
    assert!(out.items_removed.contains(&second), "{out:?}");
    let tops = shows(&e);
    assert_eq!(tops.len(), 1, "{tops:?}");
    assert_eq!(tops[0].id, first);
    let eps = episodes(&e, first);
    assert_eq!(eps.len(), 3);
    for ep in &eps {
        assert_eq!(e.detail(ep.id).assets.len(), 2, "two versions of {}", ep.title);
    }
    assert!(!exists(&e, second));
    let e2 = eps[1].id;
    let p = e
        .cat
        .read_blocking(move |c| read::progress(c, u, e2))
        .unwrap()
        .expect("the resume point followed its file");
    assert_eq!(p.position_ms, 120_000);
    let art = e.cat.read_blocking(move |c| read::item_art(c, first)).unwrap();
    assert!(art.iter().any(|a| a.id == old_poster), "{art:?}");
    assert_eq!(
        queued_art_jobs(&e, first),
        1,
        "its poster carries the other show's title"
    );
    fts_ok(&e);
}

/// An older parser took release tags for episode titles. The re-parse
/// gives those episodes their placeholder back; a title from a provider
/// or the user stays.
#[test]
fn reparse_drops_episode_titles_its_old_parse_took_from_tags() {
    let e = env(LibraryKind::Shows);
    let path = |n: u16| format!("Quiet Harbor/S01/Quiet.Harbor.S01E0{n}.Tagword.mkv");
    let tagged = |n: u16| {
        let mut g = episode("Quiet Harbor", None, 1, &[n]);
        g.episode_title = Some("Tagword".into());
        g
    };
    e.apply((1..=3).map(|n| upsert(&path(n), tagged(n), None)).collect());
    let show = e.find_top("Quiet Harbor").id;
    let eps = episodes(&e, show);
    assert!(eps.iter().all(|ep| ep.title == "Tagword"), "{eps:?}");
    sql(
        &e,
        "UPDATE item SET title = 'The Pier', sort_title = 'pier', match_state = 'matched' WHERE id = ?1",
        eps[1].id,
    );
    sql(
        &e,
        "UPDATE item SET match_state = 'locked' WHERE id = ?1",
        eps[2].id,
    );
    e.cat.writer().rebuild_search_index().wait().unwrap();

    let out = e.apply(
        (1..=3)
            .map(|n| upsert(&path(n), episode("Quiet Harbor", None, 1, &[n]), None))
            .collect(),
    );
    let titles: Vec<String> = episodes(&e, show).into_iter().map(|ep| ep.title).collect();
    assert_eq!(titles, ["Episode 1", "The Pier", "Tagword"]);
    assert!(out.items_updated.contains(&eps[0].id), "{out:?}");
    let hits = e
        .cat
        .read_blocking(|c| read::search(c, "Tagword", 10, None))
        .unwrap();
    assert_eq!(hits.len(), 1, "only the locked one: {hits:?}");
    fts_ok(&e);
}

/// An older parser kept the dots of a mixed space/dot name. The re-parse
/// retitles the movie (same work: the normalised title is the same) and
/// queues its art job, as the poster carries the title. A title a
/// provider gave is kept.
#[test]
fn reparse_retitles_a_work_named_by_its_old_parse_and_queues_its_art() {
    let e = env(LibraryKind::Movies);
    let film = "Paper Kite 2 (2024)/PK-7 Paper.Kite.2.2024.1080p.mkv";
    let other = "Lantern Field (2011)/Lantern.Field.2011.mkv";
    e.apply(vec![
        upsert(film, movie("PK-7 Paper.Kite.2", Some(2024)), None),
        upsert(other, movie("Lantern.Field", Some(2011)), None),
    ]);
    let kite = e.find_top("PK-7 Paper.Kite.2").id;
    let lantern = e.find_top("Lantern.Field").id;
    sql(
        &e,
        "UPDATE item SET title = 'Lantern Field', sort_title = 'lantern field', match_state = 'matched' WHERE id = ?1",
        lantern,
    );
    e.cat.writer().rebuild_search_index().wait().unwrap();
    finish_all_jobs(&e);

    let out = e.apply(vec![
        upsert(film, movie("PK-7 Paper Kite 2", Some(2024)), None),
        upsert(other, movie("Lantern-Field", Some(2011)), None),
    ]);
    assert_eq!(e.detail(kite).summary.title, "PK-7 Paper Kite 2");
    assert!(out.items_updated.contains(&kite), "{out:?}");
    assert_eq!(
        e.detail(lantern).summary.title,
        "Lantern Field",
        "a provider's title stays"
    );
    assert_eq!(queued_art_jobs(&e, kite), 1);
    assert_eq!(queued_art_jobs(&e, lantern), 0);
    let hits = e
        .cat
        .read_blocking(|c| read::search(c, "paper kite", 10, None))
        .unwrap();
    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), [kite]);
    fts_ok(&e);
}

/// A build between two releases re-parsed an older catalog and stored the
/// new guesses without retitling, so a later re-parse has no old title to
/// go by: old and new guess are the same. Every re-parsed item whose title
/// none of its files gives takes the one they give, as in a fresh scan:
/// an episode named after a release tag gets its placeholder, or the
/// episode title another of its files gives; a dotted show or movie title
/// gets its parsed one, preferring a file that keeps the work's
/// normalised title. A provider's or the user's title stays, and a
/// retitled show or movie gets its art job. A second re-parse changes
/// nothing.
#[test]
fn reparse_heals_titles_no_file_gives_when_the_old_guess_is_the_new_one() {
    let e = env(LibraryKind::Mixed);
    let ep_path = |n: u16| format!("Quiet Harbor/S01/Quiet.Harbor.S01E0{n}.Tagword.mkv");
    let named_e2 = "Quiet Harbor/S01/Quiet.Harbor.S01E02.The.Pier.mkv";
    let named = || {
        let mut g = episode("Quiet Harbor", None, 1, &[2]);
        g.episode_title = Some("The Pier".into());
        g
    };
    let kite = "Paper Kite 2 (2024)/PK-7 Paper.Kite.2.2024.1080p.mkv";
    let lantern = "Lantern Field (2011)/Lantern.Field.2011.mkv";
    // Two films one provider id makes one work; the first file's name
    // gives another title than the work's.
    let alias = "Copper Sky (2019)/Sky of Copper.mkv";
    let copper = "Copper Sky (2019)/Copper Sky (2019).mkv";
    let files = move || {
        let mut v: Vec<_> = (1..=3)
            .map(|n| upsert(&ep_path(n), episode("Quiet Harbor", None, 1, &[n]), None))
            .collect();
        v.push(upsert(named_e2, named(), None));
        v.push(upsert(kite, movie("PK-7 Paper Kite 2", Some(2024)), None));
        v.push(upsert(lantern, movie("Lantern Field", Some(2011)), None));
        v.push(upsert(alias, movie_tmdb("Sky of Copper", Some(2019), 77), None));
        v.push(upsert(copper, movie_tmdb("Copper Sky", Some(2019), 77), None));
        v
    };
    // The episode's untitled file comes first (the lower file id); its
    // titled one, scanned later, replaces the placeholder.
    let mut first_scan = files();
    let titled = first_scan.remove(3);
    e.apply(first_scan);
    e.apply(vec![titled]);
    let show = e.find_top("Quiet Harbor").id;
    let eps = episodes(&e, show);
    assert_eq!(eps.len(), 3);
    assert_eq!(eps[1].title, "The Pier", "a real title replaces the placeholder");
    let film = e.find_top("PK-7 Paper Kite 2").id;
    let field = e.find_top("Lantern Field").id;
    let sky = e.find_top("Sky of Copper").id;
    assert_eq!(e.detail(sky).assets.len(), 2);

    // What the old parse titled them, kept through the re-parse that
    // stored the guesses above.
    for ep in &eps {
        sql(
            &e,
            "UPDATE item SET title = 'Tagword', sort_title = 'tagword' WHERE id = ?1",
            ep.id,
        );
    }
    sql(
        &e,
        "UPDATE item SET match_state = 'locked' WHERE id = ?1",
        eps[2].id,
    );
    sql(
        &e,
        "UPDATE item SET title = 'Quiet.Harbor', sort_title = 'quiet.harbor' WHERE id = ?1",
        show,
    );
    sql(
        &e,
        "UPDATE item SET title = 'PK-7 Paper.Kite.2', sort_title = 'pk-7 paper.kite.2' WHERE id = ?1",
        film,
    );
    sql(
        &e,
        "UPDATE item SET title = 'Lantern Field: Director''s Cut', sort_title = 'lantern field',
                match_state = 'matched' WHERE id = ?1",
        field,
    );
    sql(
        &e,
        "UPDATE item SET title = 'Copper.Sky', sort_title = 'copper.sky', group_key = 'copper sky' WHERE id = ?1",
        sky,
    );
    e.cat.writer().rebuild_search_index().wait().unwrap();
    finish_all_jobs(&e);

    // The re-parse reads every name as the stored guess does.
    let out = e.apply(files());
    let titles: Vec<String> = episodes(&e, show).into_iter().map(|ep| ep.title).collect();
    assert_eq!(titles, ["Episode 1", "The Pier", "Tagword"]);
    assert_eq!(e.detail(show).summary.title, "Quiet Harbor");
    assert_eq!(e.detail(film).summary.title, "PK-7 Paper Kite 2");
    assert_eq!(
        e.detail(sky).summary.title,
        "Copper Sky",
        "the file that keeps the work's normalised title names it"
    );
    assert_eq!(
        e.detail(field).summary.title,
        "Lantern Field: Director's Cut",
        "a provider's title stays"
    );
    for id in [show, film, sky, eps[0].id, eps[1].id] {
        assert!(out.items_updated.contains(&id), "{id}: {out:?}");
    }
    for id in [show, film, sky] {
        assert_eq!(queued_art_jobs(&e, id), 1, "{id}: its poster carries the title");
    }
    assert_eq!(queued_art_jobs(&e, field), 0);
    let search = |q: &'static str| -> Vec<Id> {
        e.cat
            .read_blocking(move |c| read::search(c, q, 10, None))
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect()
    };
    assert_eq!(search("Tagword"), [eps[2].id], "only the locked one");
    assert_eq!(search("pier"), [eps[1].id]);
    assert_eq!(search("paper kite"), [film]);
    fts_ok(&e);

    finish_all_jobs(&e);
    e.apply(files());
    let again: Vec<String> = episodes(&e, show).into_iter().map(|ep| ep.title).collect();
    assert_eq!(again, titles);
    for id in [show, film, sky, field] {
        assert_eq!(queued_art_jobs(&e, id), 0, "{id}: nothing changed");
    }
}

/// A catalog a version-2 build parsed counts as outdated, so the server's
/// startup Deep scan re-parses it once more and the rules above heal it.
#[test]
fn a_catalog_a_version_2_build_parsed_is_re_parsed() {
    let e = env(LibraryKind::Movies);
    let path = "Copper Sky (2019)/Copper Sky (2019).mkv";
    e.apply(vec![upsert(path, movie("Copper Sky", Some(2019)), None)]);
    let lib = e.lib.id;
    let outdated = || {
        e.cat
            .read_blocking(move |c| read::outdated_files(c, lib))
            .unwrap()
    };
    assert_eq!(outdated(), 0);
    sql(
        &e,
        "UPDATE file SET parse_version = 2 WHERE id = ?1",
        e.file_id(path),
    );
    assert_eq!(outdated(), 1);
}

/// A work that takes a year from a file queues its art job, as flux's own
/// poster carries the year: a yearless film joined by a dated version,
/// and an undated show whose subfolder lies in a dated folder of the same
/// title.
#[test]
fn a_work_that_adopts_a_year_queues_its_art_job() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert(
        "Copper Sky/Copper Sky.mkv",
        movie("Copper Sky", None),
        None,
    )]);
    let m = e.find_top("Copper Sky").id;
    e.cat
        .writer()
        .add_art(
            m,
            frame(
                ArtKind::Poster,
                "art/1-poster-0000000a.jpg",
                Some(1),
                Some(&read::art_caption("Copper Sky", None)),
            ),
        )
        .wait()
        .unwrap();
    finish_all_jobs(&e);
    let stale = |e: &Env, id: Id| {
        e.cat
            .read_blocking(move |c| read::stale_art(c, 1, Some(id)))
            .unwrap()
    };
    assert!(stale(&e, m).is_empty());
    e.apply(vec![upsert(
        "Copper Sky (2019)/Copper Sky (2019).mkv",
        movie("Copper Sky", Some(2019)),
        None,
    )]);
    let d = e.detail(m);
    assert_eq!((d.summary.year, d.assets.len()), (Some(2019), 2));
    assert_eq!(stale(&e, m).len(), 1, "the poster says Copper Sky");
    assert_eq!(queued_art_jobs(&e, m), 1);

    let e = env(LibraryKind::Shows);
    let file = |dir: &str, year: Option<u16>, n: u16| {
        upsert(
            &format!("{dir}/Starhold.S01E0{n}.mkv"),
            episode("Starhold", year, 1, &[n]),
            None,
        )
    };
    e.apply((1..=2).map(|n| file("Starhold (2004)/WEB", None, n)).collect());
    let show = e.find_top("Starhold").id;
    e.cat
        .writer()
        .add_art(
            show,
            frame(
                ArtKind::Poster,
                "art/2-poster-0000000b.jpg",
                Some(1),
                Some(&read::art_caption("Starhold", None)),
            ),
        )
        .wait()
        .unwrap();
    finish_all_jobs(&e);
    e.apply((1..=2).map(|n| file("Starhold (2004)", Some(2004), n)).collect());
    assert_eq!(shows(&e).len(), 1);
    assert_eq!(e.detail(show).summary.year, Some(2004));
    assert_eq!(stale(&e, show).len(), 1);
    assert_eq!(queued_art_jobs(&e, show), 1);
}

/// A work that another's file moved to takes that work's locked match
/// (title and year), or a merge gives it the year it lacked: its poster's
/// caption is out of date, so its art job is queued, although the other
/// work had no image of flux's own to bring.
#[test]
fn a_carried_match_or_merge_that_recaptions_a_work_queues_its_art_job() {
    let e = env(LibraryKind::Movies);
    let poster = |e: &Env, id: Id, caption: String| {
        e.cat
            .writer()
            .add_art(
                id,
                frame(
                    ArtKind::Poster,
                    "art/3-poster-0000000c.jpg",
                    Some(1),
                    Some(&caption),
                ),
            )
            .wait()
            .unwrap();
    };
    let tagged = "Copper Sky (2019)/Copper.Sky.2019.Tagword.mkv";
    e.apply(vec![
        upsert(
            "Copper Sky (2019)/Copper Sky (2019).mkv",
            movie("Copper Sky", Some(2019)),
            None,
        ),
        upsert(tagged, movie("Copper Sky Tagword", Some(2019)), None),
    ]);
    let target = e.find_top("Copper Sky").id;
    let locked = e.find_top("Copper Sky Tagword").id;
    poster(&e, target, read::art_caption("Copper Sky", Some(2019)));
    sql(
        &e,
        "UPDATE item SET title = 'Copper Sky Redux', sort_title = 'copper sky redux', match_state = 'locked'
          WHERE id = ?1",
        locked,
    );
    e.cat.writer().rebuild_search_index().wait().unwrap();
    finish_all_jobs(&e);
    let out = e.apply(vec![upsert(tagged, movie("Copper Sky", Some(2019)), None)]);
    assert!(out.items_removed.contains(&locked), "{out:?}");
    assert_eq!(e.detail(target).summary.title, "Copper Sky Redux");
    assert_eq!(queued_art_jobs(&e, target), 1);

    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("Copper Sky/Copper Sky.mkv", movie("Copper Sky", None), None),
        upsert(
            "Sky of Copper (2019)/Sky of Copper (2019).mkv",
            movie("Sky of Copper", Some(2019)),
            None,
        ),
    ]);
    let into = e.find_top("Copper Sky").id;
    let from = e.find_top("Sky of Copper").id;
    poster(&e, into, read::art_caption("Copper Sky", None));
    finish_all_jobs(&e);
    e.cat.writer().merge_items(from, into).wait().unwrap();
    assert_eq!(e.detail(into).summary.year, Some(2019));
    assert_eq!(queued_art_jobs(&e, into), 1);
}

/// A film the old parser cataloged as a film of its own is an extra of
/// another film now. The emptied film goes, with its images; its users'
/// state moves to the extra. Nothing is left hidden.
#[test]
fn reparse_that_makes_a_film_an_extra_deletes_the_emptied_film() {
    let e = env(LibraryKind::Movies);
    let trailer = "Paper Kite (2003)/Paper Kite-trailer.mkv";
    e.apply(vec![
        upsert(
            "Paper Kite (2003)/Paper Kite (2003).mkv",
            movie("Paper Kite", Some(2003)),
            None,
        ),
        upsert(trailer, movie("Paper Kite trailer", None), None),
    ]);
    let film = e.find_top("Paper Kite").id;
    let old = e.find_top("Paper Kite trailer").id;
    let u = user(&e);
    e.cat
        .writer()
        .set_progress(u, old, None, 30_000, 120_000)
        .wait()
        .unwrap();
    e.cat
        .writer()
        .add_art(
            old,
            frame(ArtKind::Poster, "art/2-poster-0000000b.jpg", None, None),
        )
        .wait()
        .unwrap();

    let out = e.apply(vec![upsert(
        trailer,
        extra(Some("Paper Kite"), Some(2003), ExtraKind::Trailer),
        None,
    )]);
    assert!(out.items_removed.contains(&old), "{out:?}");
    assert!(!exists(&e, old), "the emptied film is deleted, not left missing");
    let tops: Vec<String> = e.top().into_iter().map(|i| i.title).collect();
    assert_eq!(tops, ["Paper Kite"]);
    let extras = e.children(film);
    assert_eq!(extras.len(), 1, "{extras:?}");
    let x = extras[0].id;
    let p = e
        .cat
        .read_blocking(move |c| read::progress(c, u, x))
        .unwrap()
        .expect("state carried to the extra");
    assert_eq!(p.position_ms, 30_000);
    let art_left: i64 = e
        .cat
        .read_blocking(move |c| {
            Ok(c.query_row("SELECT count(*) FROM art WHERE item_id = ?1", [old], |r| r.get(0))?)
        })
        .unwrap();
    assert_eq!(art_left, 0);
    fts_ok(&e);
}

/// What an earlier upgrade left behind: items with no file anywhere
/// below them (hidden as missing), and an empty season of a live show.
#[test]
fn purge_deletes_items_left_without_files() {
    let e = env(LibraryKind::Mixed);
    e.apply(vec![
        upsert(
            "Copper Sky (2019)/Copper Sky (2019).mkv",
            movie("Copper Sky", Some(2019)),
            None,
        ),
        upsert(
            "Tide Line/S01/Tide.Line.S01E01.mkv",
            episode("Tide Line", None, 1, &[1]),
            None,
        ),
        upsert(
            "Orphan Film (2001)/Orphan Film (2001).mkv",
            movie("Orphan Film", Some(2001)),
            None,
        ),
    ]);
    let show = e.find_top("Tide Line").id;
    let orphan = e.find_top("Orphan Film").id;
    let u = user(&e);
    e.cat
        .writer()
        .set_progress(u, orphan, None, 5_000, 100_000)
        .wait()
        .unwrap();
    e.cat
        .writer()
        .add_art(
            orphan,
            frame(ArtKind::Poster, "art/3-poster-0000000c.jpg", None, None),
        )
        .wait()
        .unwrap();
    // The orphan's file went elsewhere long ago; an empty season remains.
    sql(&e, "DELETE FROM asset WHERE item_id = ?1", orphan);
    sql(
        &e,
        "INSERT INTO item(library_id, kind, parent_id, group_key, title, sort_title, season_number,
                          added_at, content_added_at, updated_at)
         SELECT library_id, 'season', id, 's2', 'Season 2', 'season 000002', 2, 0, 0, 0 FROM item WHERE id = ?1",
        show,
    );
    let deleted = e.cat.writer().purge_empty_items().wait().unwrap();
    assert_eq!(deleted.len(), 2, "{deleted:?}");
    assert!(deleted.contains(&orphan));
    assert!(!exists(&e, orphan));
    assert!(exists(&e, show));
    assert_eq!(e.children(show).len(), 1, "season 1 stays");
    let tops: Vec<String> = e.top().into_iter().map(|i| i.title).collect();
    assert_eq!(tops.len(), 2, "{tops:?}");
    assert_eq!(
        e.cat.writer().purge_empty_items().wait().unwrap(),
        Vec::<Id>::new()
    );
    fts_ok(&e);
}

/// flux's own images made by an older art version (or before versioning),
/// or posters of another title, are stale; provider, local and pinned
/// images never are. A stale image of a kind that has a fresh one is what
/// an art job deletes.
#[test]
fn stale_art_is_flux_art_of_an_older_version_or_another_title() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert(
        "Copper Sky (2019)/Copper Sky (2019).mkv",
        movie("Copper Sky", Some(2019)),
        None,
    )]);
    let m = e.find_top("Copper Sky").id;
    let w = e.cat.writer();
    let add = |a: NewArt| w.add_art(m, a).wait().unwrap();
    let unversioned = add(frame(ArtKind::Poster, "art/1-poster-00000001.jpg", None, None));
    let current = add(frame(
        ArtKind::Poster,
        "art/1-poster-00000002.jpg",
        Some(1),
        Some(&read::art_caption("Copper Sky", Some(2019))),
    ));
    let provider = add(NewArt {
        source: ArtSource::Provider,
        path: None,
        url: Some("https://image.example/p.jpg".into()),
        ..frame(ArtKind::Poster, "", None, None)
    });
    let local = add(NewArt {
        source: ArtSource::Local,
        path: Some("/media/Copper Sky (2019)/poster.jpg".into()),
        ..frame(ArtKind::Poster, "", None, None)
    });
    let old_backdrop = add(frame(
        ArtKind::Backdrop,
        "art/1-backdrop-00000003.jpg",
        Some(0),
        None,
    ));
    let pinned = add(frame(ArtKind::Thumb, "art/1-thumb-00000004.jpg", None, None));
    w.pin_art(pinned).wait().unwrap();
    assert_eq!(read::art_caption("Copper Sky", None), "Copper Sky");

    let stale = |work: Option<Id>| -> Vec<Id> {
        e.cat
            .read_blocking(move |c| read::stale_art(c, 1, work))
            .unwrap()
            .into_iter()
            .map(|s| {
                assert_eq!(s.work_id, m);
                s.art_id
            })
            .collect()
    };
    assert_eq!(stale(None), [unversioned, old_backdrop]);
    assert_eq!(stale(Some(m)), [unversioned, old_backdrop]);
    let fresh = |kind| {
        e.cat
            .read_blocking(move |c| read::has_fresh_art(c, m, kind, 1))
            .unwrap()
    };
    assert!(fresh(ArtKind::Poster));
    assert!(!fresh(ArtKind::Backdrop));
    let replaced: Vec<Id> = e
        .cat
        .read_blocking(move |c| read::replaced_stale_art(c, 1, m))
        .unwrap()
        .into_iter()
        .map(|s| s.art_id)
        .collect();
    assert_eq!(replaced, [unversioned], "the backdrop has no replacement yet");

    // One refresh job per work; a restart doesn't queue it again.
    finish_all_jobs(&e);
    assert_eq!(w.refresh_stale_art(1).wait().unwrap(), 1);
    assert_eq!(queued_art_jobs(&e, m), 1);
    finish_all_jobs(&e);
    assert_eq!(w.refresh_stale_art(1).wait().unwrap(), 0);

    // A match that renames the movie: its current poster is stale too,
    // and its art job is queued.
    w.update_item_metadata(
        m,
        flux_catalog::ItemMetadata {
            title: Some("Copper Sky Redux".into()),
            ..Default::default()
        },
    )
    .wait()
    .unwrap();
    assert_eq!(stale(None), [unversioned, current, old_backdrop]);
    assert_eq!(queued_art_jobs(&e, m), 1);
    let gone = w.remove_arts(vec![unversioned, current, 999_999]).wait().unwrap();
    assert_eq!(gone, ["art/1-poster-00000001.jpg", "art/1-poster-00000002.jpg"]);
    let left: Vec<Id> = e
        .cat
        .read_blocking(move |c| read::item_art(c, m))
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    for id in [provider, local, old_backdrop, pinned] {
        assert!(left.contains(&id), "{id} kept: {left:?}");
    }
}

/// An episode whose only thumbnail is stale counts as having none.
#[test]
fn episodes_with_only_stale_thumbs_need_new_ones() {
    let e = env(LibraryKind::Shows);
    e.apply(
        (1..=2)
            .map(|n| {
                upsert(
                    &format!("Tide Line/S01/Tide.Line.S01E0{n}.mkv"),
                    episode("Tide Line", None, 1, &[n]),
                    None,
                )
            })
            .collect(),
    );
    let show = e.find_top("Tide Line").id;
    let eps = episodes(&e, show);
    let w = e.cat.writer();
    w.add_art(
        eps[0].id,
        frame(ArtKind::Thumb, "art/5-thumb-00000005.jpg", None, None),
    )
    .wait()
    .unwrap();
    w.add_art(
        eps[1].id,
        frame(ArtKind::Thumb, "art/6-thumb-00000006.jpg", Some(1), None),
    )
    .wait()
    .unwrap();
    let got = e
        .cat
        .read_blocking(move |c| read::show_episodes_fresh_art(c, show, ArtKind::Thumb, 1))
        .unwrap();
    assert_eq!(got, [(eps[0].id, false), (eps[1].id, true)]);
    let all = e
        .cat
        .read_blocking(move |c| read::show_episodes_art(c, show, ArtKind::Thumb))
        .unwrap();
    assert_eq!(all, [(eps[0].id, true), (eps[1].id, true)]);
}
