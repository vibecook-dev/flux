//! Structure fixes (docs/AGENT-NATIVE.md): what a file is when its name says
//! otherwise. Merges, splits, renumbered episodes, extras, ignored files;
//! they hold through rescans, and undo gives back what a merge removed.

mod common;

use common::*;
use flux_catalog::changes::{Actor, ItemEdit};
use flux_catalog::fixes::{self, FixEdit};
use flux_catalog::{changes, fields, read};
use flux_core::agent::{ChangeState, ChangesetState, FileFix, FixKind};
use flux_core::api::{Id, Identity, ItemKind, Role};
use flux_core::{ExtraKind, LibraryKind};
use serde_json::json;

fn agent() -> Actor {
    Actor {
        login: "james@example.com".into(),
        agent: Some("claude-code".into()),
    }
}

fn user(e: &Env, login: &str) -> Id {
    e.cat
        .writer()
        .ensure_user(Identity {
            login_name: login.into(),
            display_name: login.into(),
            profile_pic_url: None,
            node_name: None,
            role: Role::Owner,
            tags: Vec::new(),
        })
        .wait()
        .unwrap()
}

fn fix(file: Id, f: FileFix) -> FixEdit {
    FixEdit {
        file,
        fix: Some(f),
        sources: Vec::new(),
    }
}

impl Env {
    fn fix(&self, edits: Vec<FixEdit>) -> (changes::Committed, Vec<fixes::FixOutcome>) {
        self.fix_with(edits, false, false)
    }

    fn fix_with(
        &self,
        edits: Vec<FixEdit>,
        propose: bool,
        dry_run: bool,
    ) -> (changes::Committed, Vec<fixes::FixOutcome>) {
        self.cat
            .writer()
            .commit_fixes(agent(), "test".into(), edits, propose, dry_run, Vec::new())
            .wait()
            .unwrap()
    }

    fn undo(&self, cs: Id) -> changes::Committed {
        self.cat
            .writer()
            .revert_changeset(cs, "james@example.com".into())
            .wait()
            .unwrap()
    }

    fn titles(&self) -> Vec<String> {
        let mut t: Vec<String> = self.top().into_iter().map(|i| i.title).collect();
        t.sort();
        t
    }

    fn item_of_file(&self, path: &str) -> Id {
        let file = self.file_id(path);
        self.cat
            .read_blocking(move |c| Ok(fixes::place_of(c, file, &[])?.0.unwrap()))
            .unwrap()
    }

    fn watched(&self, user: Id, item: Id) -> bool {
        self.cat
            .read_blocking(move |c| {
                Ok(c.query_row(
                    "SELECT COALESCE((SELECT watched FROM user_state WHERE user_id = ?1 AND item_id = ?2), 0)",
                    rusqlite::params![user, item],
                    |r| r.get::<_, i64>(0),
                )? == 1)
            })
            .unwrap()
    }
}

fn cs_id(c: &changes::Committed) -> Id {
    c.changeset.as_ref().expect("a changeset").id
}

#[test]
fn merging_two_films_holds_through_a_rescan_and_undo_gives_back_the_film() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert(
            "Interstellar.2014.1080p.mkv",
            movie("Interstellar", Some(2014)),
            None,
        ),
        upsert("星际穿越4k.mp4", movie("星际穿越4k", None), None),
    ]);
    let keep = e.find_top("Interstellar").id;
    let other = e.find_top("星际穿越4k").id;
    // Someone set things on the one that goes: they come along, and back.
    e.cat
        .writer()
        .commit_edit(
            agent(),
            "tags".into(),
            vec![ItemEdit {
                item: other,
                set: [("tags".to_string(), json!(["4K"]))].into(),
                ..ItemEdit::default()
            }],
            false,
            false,
        )
        .wait()
        .unwrap();
    let james = user(&e, "james@example.com");
    e.cat.writer().set_watched(james, other, true).wait().unwrap();

    let edits = e
        .cat
        .read_blocking(move |c| fixes::merge_edits(c, keep, &[other], &[]))
        .unwrap();
    assert_eq!(edits.len(), 1);
    let (out, files) = e.fix(edits);
    let cs = cs_id(&out);
    assert_eq!(out.done, 1);
    assert_eq!(files[0].before.0, Some(other));
    assert_eq!(files[0].after.as_ref().unwrap().0, Some(keep));
    assert!(out.removed.iter().any(|(i, _)| *i == other), "{out:?}");
    assert_eq!(e.titles(), ["Interstellar"]);
    assert_eq!(e.detail(keep).assets.len(), 2);
    assert!(e.watched(james, keep), "watched state comes along");
    let tags = e
        .cat
        .read_blocking(move |c| Ok(fields::read(c, keep)?.unwrap().values.get("tags").cloned()))
        .unwrap();
    assert_eq!(tags, Some(json!(["4K"])));

    // A rescan that reads the file again keeps it where the fix put it.
    e.apply(vec![upsert("星际穿越4k.mp4", movie("星际穿越4k", None), None)]);
    assert_eq!(e.titles(), ["Interstellar"]);
    assert_eq!(e.item_of_file("星际穿越4k.mp4"), keep);

    // Undo: the file is a film of its own again, with what it had.
    let undone = e.undo(cs);
    assert_eq!(undone.done, 1, "{:?}", undone.not_done);
    assert_eq!(e.titles(), ["Interstellar", "星际穿越4k"]);
    let back = e.item_of_file("星际穿越4k.mp4");
    assert_ne!(back, keep);
    assert!(e.watched(james, back), "its watched state is back");
    let tags = e
        .cat
        .read_blocking(move |c| Ok(fields::read(c, back)?.unwrap().values.get("tags").cloned()))
        .unwrap();
    assert_eq!(tags, Some(json!(["4K"])), "and what the agent set on it");
    let file = e.file_id("星际穿越4k.mp4");
    assert_eq!(
        e.cat.read_blocking(move |c| fixes::fix_of(c, file)).unwrap(),
        None
    );
    let s = e
        .cat
        .read_blocking(move |c| changes::get(c, cs))
        .unwrap()
        .unwrap();
    assert_eq!(s.0.state, ChangesetState::Reverted);
    assert_eq!(s.1[0].state, ChangeState::Reverted);
    assert!(
        s.1[0].before.as_ref().unwrap().get("keep").is_none(),
        "the journal's own"
    );
}

#[test]
fn two_films_in_one_item_split_by_year() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("The Thing/The Thing.mkv", movie("The Thing", None), None),
        upsert("The Thing/The Thing (remake).mkv", movie("The Thing", None), None),
    ]);
    assert_eq!(e.titles(), ["The Thing"]);
    let a = e.file_id("The Thing/The Thing.mkv");
    let b = e.file_id("The Thing/The Thing (remake).mkv");
    let (out, _) = e.fix(vec![
        fix(
            a,
            FileFix {
                year: Some(1982),
                ..FileFix::default()
            },
        ),
        fix(
            b,
            FileFix {
                year: Some(2011),
                ..FileFix::default()
            },
        ),
    ]);
    assert_eq!(out.done, 2);
    let mut years: Vec<Option<u16>> = e.top().into_iter().map(|i| i.year).collect();
    years.sort();
    assert_eq!(years, [Some(1982), Some(2011)]);
    assert_ne!(
        e.item_of_file("The Thing/The Thing.mkv"),
        e.item_of_file("The Thing/The Thing (remake).mkv")
    );
    // Undo puts them back together.
    e.undo(cs_id(&out));
    assert_eq!(e.titles(), ["The Thing"]);
}

#[test]
fn a_renumbered_episode_moves_with_its_watched_state() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert(
            "Show/Season 1/Show - S01E01.mkv",
            episode("Show", None, 1, &[1]),
            None,
        ),
        upsert(
            "Show/Season 1/Show - S01E17.mkv",
            episode("Show", None, 1, &[17]),
            None,
        ),
    ]);
    let wrong = e.item_of_file("Show/Season 1/Show - S01E17.mkv");
    let james = user(&e, "james@example.com");
    e.cat.writer().set_watched(james, wrong, true).wait().unwrap();
    let file = e.file_id("Show/Season 1/Show - S01E17.mkv");
    let (out, files) = e.fix(vec![fix(
        file,
        FileFix {
            season: Some(2),
            episodes: Some(vec![5]),
            ..FileFix::default()
        },
    )]);
    assert_eq!(out.done, 1);
    let title = files[0].after.as_ref().unwrap().1.clone().unwrap();
    assert!(title.starts_with("Show · S02E05"), "{title}");
    let right = e.item_of_file("Show/Season 1/Show - S01E17.mkv");
    assert_ne!(right, wrong);
    assert!(e.watched(james, right));
    let show = e.find_top("Show").id;
    let seasons: Vec<String> = e.children(show).into_iter().map(|s| s.title).collect();
    assert_eq!(seasons.len(), 2, "{seasons:?}");
}

#[test]
fn a_trailer_taken_for_a_film_becomes_its_extra_and_back() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("Interstellar (2014).mkv", movie("Interstellar", Some(2014)), None),
        upsert(
            "Interstellar Teaser.mkv",
            movie("Interstellar Teaser", None),
            None,
        ),
    ]);
    let film = e.find_top("Interstellar").id;
    let file = e.file_id("Interstellar Teaser.mkv");
    let (out, _) = e.fix(vec![fix(
        file,
        FileFix {
            kind: Some(FixKind::Extra),
            extra: Some(ExtraKind::Trailer),
            of: Some(film),
            ..FileFix::default()
        },
    )]);
    assert_eq!(e.titles(), ["Interstellar"]);
    let kids = e.children(film);
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].kind, ItemKind::Extra);
    e.undo(cs_id(&out));
    assert_eq!(e.titles(), ["Interstellar", "Interstellar Teaser"]);
}

#[test]
fn an_ignored_file_shows_nowhere_until_its_fix_is_cleared() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert("junk/clip0001.mkv", movie("clip0001", None), None)]);
    let file = e.file_id("junk/clip0001.mkv");
    e.fix(vec![fix(
        file,
        FileFix {
            kind: Some(FixKind::Ignore),
            ..FileFix::default()
        },
    )]);
    assert!(e.titles().is_empty());
    // A rescan keeps it out.
    e.apply(vec![upsert("junk/clip0001.mkv", movie("clip0001", None), None)]);
    assert!(e.titles().is_empty());
    let (out, files) = e.fix(vec![FixEdit {
        file,
        fix: None,
        sources: Vec::new(),
    }]);
    assert_eq!(out.done, 1);
    assert_eq!(files[0].fix, None);
    assert_eq!(e.titles(), ["clip0001"]);
}

#[test]
fn a_dry_run_says_where_files_go_and_changes_nothing() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("A.mkv", movie("A", None), None),
        upsert("B.mkv", movie("B", None), None),
    ]);
    let a = e.find_top("A").id;
    let file = e.file_id("B.mkv");
    let (out, files) = e.fix_with(
        vec![fix(
            file,
            FileFix {
                of: Some(a),
                ..FileFix::default()
            },
        )],
        false,
        true,
    );
    assert!(out.changeset.is_none());
    assert_eq!(files[0].after.as_ref().unwrap().0, Some(a));
    assert_eq!(e.titles(), ["A", "B"]);
    assert_eq!(
        e.cat.read_blocking(move |c| fixes::fix_of(c, file)).unwrap(),
        None
    );
    let n = e
        .cat
        .read_blocking(|c| Ok(changes::list(c, None, None, None, 10)?.len()))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn a_proposed_fix_applies_later_unless_the_file_changed() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("A.mkv", movie("A", None), None),
        upsert("B.mkv", movie("B", None), None),
        upsert("C.mkv", movie("C", None), None),
    ]);
    let a = e.find_top("A").id;
    let (b, c) = (e.file_id("B.mkv"), e.file_id("C.mkv"));
    let pin = |of| FileFix {
        of: Some(of),
        ..FileFix::default()
    };
    let (out, files) = e.fix_with(vec![fix(b, pin(a)), fix(c, pin(a))], true, false);
    let cs = cs_id(&out);
    assert!(files.iter().all(|f| f.after.is_none()), "known once applied");
    assert_eq!(e.titles(), ["A", "B", "C"], "nothing changed yet");
    // C gets another fix meanwhile.
    e.fix(vec![fix(
        c,
        FileFix {
            year: Some(1999),
            ..FileFix::default()
        },
    )]);
    let applied = e
        .cat
        .writer()
        .apply_proposal(cs, "james@example.com".into())
        .wait()
        .unwrap();
    assert_eq!(applied.done, 1);
    assert_eq!(applied.not_done.len(), 1);
    assert!(
        applied.not_done[0].2.contains("changed since"),
        "{:?}",
        applied.not_done
    );
    assert_eq!(e.titles(), ["A", "C"]);
}

#[test]
fn fixes_that_make_no_sense_are_refused() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("A.mkv", movie("A", None), None),
        upsert("Show/Show - S01E01.mkv", episode("Show", None, 1, &[1]), None),
    ]);
    let show = e.find_top("Show").id;
    let file = e.file_id("A.mkv");
    let refused = |f: FileFix| {
        e.cat
            .writer()
            .commit_fixes(agent(), "x".into(), vec![fix(file, f)], false, false, Vec::new())
            .wait()
            .unwrap_err()
            .to_string()
    };
    let m = refused(FileFix {
        of: Some(show),
        ..FileFix::default()
    });
    assert!(m.contains("is a show"), "{m}");
    let m = refused(FileFix {
        kind: Some(FixKind::Episode),
        ..FileFix::default()
    });
    assert!(m.contains("needs its number"), "{m}");
    let m = refused(FileFix {
        year: Some(1500),
        ..FileFix::default()
    });
    assert!(m.contains("1870"), "{m}");
    let videos = env(LibraryKind::Videos);
    videos.apply(vec![upsert("Trip/Clip.mp4", movie("Clip", None), None)]);
    let clip = videos.file_id("Trip/Clip.mp4");
    let m = videos
        .cat
        .writer()
        .commit_fixes(
            agent(),
            "x".into(),
            vec![fix(
                clip,
                FileFix {
                    title: Some("Beach".into()),
                    ..FileFix::default()
                },
            )],
            false,
            false,
            Vec::new(),
        )
        .wait()
        .unwrap_err()
        .to_string();
    assert!(m.contains("videos library"), "{m}");
}

#[test]
fn a_pin_follows_its_work_when_that_work_is_merged() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert("A.mkv", movie("A", None), None),
        upsert("B.mkv", movie("B", None), None),
        upsert("C.mkv", movie("C", None), None),
    ]);
    let (a, b) = (e.find_top("A").id, e.find_top("B").id);
    let c_file = e.file_id("C.mkv");
    // C belongs to B; then B is merged into A: C's pin follows.
    e.fix(vec![fix(
        c_file,
        FileFix {
            of: Some(b),
            ..FileFix::default()
        },
    )]);
    let edits = e
        .cat
        .read_blocking(move |c| fixes::merge_edits(c, a, &[b], &[]))
        .unwrap();
    e.fix(edits);
    assert_eq!(e.titles(), ["A"]);
    let f = e
        .cat
        .read_blocking(move |c| fixes::fix_of(c, c_file))
        .unwrap()
        .unwrap();
    assert_eq!(f.of, Some(a));
    e.apply(vec![upsert("C.mkv", movie("C", None), None)]);
    assert_eq!(e.titles(), ["A"]);
    let _ = read::item_detail;
}
