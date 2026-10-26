//! Agents' edits (docs/AGENT-NATIVE.md): where values come from, locks,
//! changesets (apply, propose, undo), what agents read, lenses.

mod common;

use common::*;
use flux_catalog::changes::{Actor, ItemEdit};
use flux_catalog::{agent_read, changes, fields, read};
use flux_core::agent::{ChangeState, ChangesetState, Gap, Link};
use flux_core::api::{ArtSource, Id};
use flux_core::LibraryKind;
use serde_json::{json, Value};

fn agent() -> Actor {
    Actor {
        login: "james@example.com".into(),
        agent: Some("claude-code".into()),
    }
}

fn person() -> Actor {
    Actor {
        login: "james@example.com".into(),
        agent: None,
    }
}

fn set(item: Id, values: &[(&str, Value)]) -> ItemEdit {
    ItemEdit {
        item,
        set: values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        clear: Vec::new(),
        sources: vec![Link {
            url: "https://en.wikipedia.org/wiki/Parasite_(2019_film)".into(),
            title: Some("Wikipedia".into()),
        }],
    }
}

fn clear(item: Id, fields: &[&str]) -> ItemEdit {
    ItemEdit {
        item,
        clear: fields.iter().map(|f| f.to_string()).collect(),
        ..ItemEdit::default()
    }
}

impl Env {
    fn edit(&self, who: Actor, edits: Vec<ItemEdit>) -> changes::Committed {
        self.edit_with(who, edits, false, false)
    }

    fn edit_with(&self, who: Actor, edits: Vec<ItemEdit>, force: bool, propose: bool) -> changes::Committed {
        self.cat
            .writer()
            .commit_edit(who, "test".into(), edits, force, propose)
            .wait()
            .unwrap()
            .0
    }

    fn values(&self, id: Id) -> fields::Values {
        self.cat
            .read_blocking(move |c| Ok(fields::read(c, id)?.unwrap().values))
            .unwrap()
    }

    fn source(&self, id: Id, field: &'static str) -> Option<fields::FieldRow> {
        self.cat
            .read_blocking(move |c| fields::source_of(c, id, field))
            .unwrap()
    }

    fn movie_at(&self, path: &str, title: &str, year: Option<u16>) -> Id {
        self.apply(vec![upsert(path, movie(title, year), None)]);
        self.find_top(title).id
    }
}

fn cs_id(c: &changes::Committed) -> Id {
    c.changeset.as_ref().expect("a changeset").id
}

#[test]
fn an_agents_edit_records_its_sources_and_undoes_cleanly() {
    let e = env(LibraryKind::Movies);
    let id = e.movie_at("Gisaengchung.2019.mkv", "Gisaengchung", Some(2019));
    let out = e.edit(
        agent(),
        vec![set(
            id,
            &[
                ("title", json!("Parasite")),
                ("original_title", json!("기생충")),
                ("directors", json!(["Bong Joon-ho"])),
                ("ids.tmdb", json!("496243")),
                ("ids.wikidata", json!("Q61448040")),
            ],
        )],
    );
    let cs = cs_id(&out);
    assert_eq!(out.done, 5);
    assert_eq!(out.items, vec![(id, e.lib.id)]);
    let s = out.changeset.unwrap();
    assert_eq!(
        (s.state, s.agent.as_deref(), s.item_count, s.change_count),
        (ChangesetState::Applied, Some("claude-code"), 1, 5)
    );

    let d = e.detail(id);
    assert_eq!(d.summary.title, "Parasite");
    assert!(d.external_ids.contains(&("tmdb".into(), "496243".into())));
    let v = e.values(id);
    assert_eq!(v["directors"], json!(["Bong Joon-ho"]));
    assert_eq!(v["ids.wikidata"], json!("Q61448040"));
    let src = e.source(id, "title").unwrap();
    assert_eq!(
        (src.source.as_str(), src.agent.as_deref(), src.changeset),
        ("agent", Some("claude-code"), Some(cs))
    );
    assert_eq!(src.sources[0].title.as_deref(), Some("Wikipedia"));
    // Search finds the new title and the original one.
    for q in ["parasite", "기생충"] {
        let hits = e
            .cat
            .read_blocking(move |c| read::search(c, q, 10, None))
            .unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), [id], "{q}");
    }

    // A provider match later fills only what nobody set.
    let filled = e
        .cat
        .writer()
        .update_item_metadata(
            id,
            flux_catalog::ItemMetadata {
                title: Some("Parasite (TMDB)".into()),
                overview: Some("A provider's description.".into()),
                tmdb_id: Some(1),
                matched: true,
                provider: Some("tmdb".into()),
                ..Default::default()
            },
        )
        .wait()
        .unwrap();
    assert!(filled);
    let d = e.detail(id);
    assert_eq!(d.summary.title, "Parasite", "the agent's title stays");
    assert!(
        d.external_ids.contains(&("tmdb".into(), "496243".into())),
        "and its id"
    );
    assert_eq!(d.overview.as_deref(), Some("A provider's description."));
    assert_eq!(
        e.source(id, "overview").unwrap().provider.as_deref(),
        Some("tmdb")
    );

    // Undo: every field the changeset set goes back, sources too.
    let undone = e
        .cat
        .writer()
        .revert_changeset(cs, "james@example.com".into())
        .wait()
        .unwrap();
    assert_eq!((undone.done, undone.not_done.len()), (5, 0));
    assert_eq!(undone.changeset.as_ref().unwrap().state, ChangesetState::Reverted);
    let d = e.detail(id);
    assert_eq!(d.summary.title, "Gisaengchung");
    assert!(!d.external_ids.iter().any(|(p, _)| p == "tmdb"));
    assert_eq!(
        d.overview.as_deref(),
        Some("A provider's description."),
        "what came after stays"
    );
    assert!(e.source(id, "title").is_none());
    assert!(!e.values(id).contains_key("directors"));
    let hits = e
        .cat
        .read_blocking(|c| read::search(c, "parasite", 10, None))
        .unwrap();
    assert!(hits.is_empty());
    let (_, rows) = e
        .cat
        .read_blocking(move |c| changes::get(c, cs))
        .unwrap()
        .unwrap();
    assert!(rows.iter().all(|r| r.state == ChangeState::Reverted));
    // Only an applied changeset can be undone.
    assert!(e.cat.writer().revert_changeset(cs, "x".into()).wait().is_err());
}

#[test]
fn undo_keeps_a_field_that_changed_since() {
    let e = env(LibraryKind::Movies);
    let id = e.movie_at("Heat.mkv", "Heat", None);
    let first = cs_id(&e.edit(
        agent(),
        vec![set(
            id,
            &[
                ("year", json!(1995)),
                ("tagline", json!("A Los Angeles crime saga.")),
            ],
        )],
    ));
    e.edit(person(), vec![set(id, &[("year", json!(1996))])]);
    let undone = e.cat.writer().revert_changeset(first, "j".into()).wait().unwrap();
    assert_eq!(undone.done, 1, "the tagline");
    assert_eq!(undone.not_done.len(), 1);
    assert_eq!(undone.not_done[0].1, "year");
    assert_eq!(e.values(id)["year"], json!(1996));
    assert!(!e.values(id).contains_key("tagline"));
    let (_, rows) = e
        .cat
        .read_blocking(move |c| changes::get(c, first))
        .unwrap()
        .unwrap();
    let states: Vec<(String, ChangeState)> = rows.iter().map(|r| (r.field.clone(), r.state)).collect();
    assert!(states.contains(&("year".into(), ChangeState::Kept)));
    assert!(states.contains(&("tagline".into(), ChangeState::Reverted)));
}

#[test]
fn an_agent_replaces_what_a_person_set_only_when_forced() {
    let e = env(LibraryKind::Movies);
    let id = e.movie_at("Brazil.mkv", "Brazil", Some(1985));
    e.edit(
        person(),
        vec![set(id, &[("title", json!("Brazil (Director's Cut)"))])],
    );
    let planned = e
        .cat
        .writer()
        .commit_edit(
            agent(),
            "x".into(),
            vec![set(
                id,
                &[
                    ("title", json!("Brazil")),
                    ("year", json!(1985)),
                    ("tags", json!(["Satire"])),
                ],
            )],
            false,
            false,
        )
        .wait()
        .unwrap()
        .1;
    let p = &planned[0];
    assert_eq!(p.refused.len(), 1);
    assert_eq!(p.refused[0].0, "title");
    assert!(p.refused[0].1.contains("by hand"), "{}", p.refused[0].1);
    assert_eq!(p.unchanged, ["year"]);
    assert_eq!(
        p.changes.iter().map(|c| c.field.as_str()).collect::<Vec<_>>(),
        ["tags"]
    );
    assert_eq!(e.detail(id).summary.title, "Brazil (Director's Cut)");
    e.edit_with(agent(), vec![set(id, &[("title", json!("Brazil"))])], true, false);
    assert_eq!(e.detail(id).summary.title, "Brazil");
    // A person may always replace an agent's value.
    e.edit(person(), vec![set(id, &[("tags", json!(["Dystopia"]))])]);
    assert_eq!(e.values(id)["tags"], json!(["Dystopia"]));
}

#[test]
fn a_proposal_applies_what_still_holds_or_is_turned_down() {
    let e = env(LibraryKind::Movies);
    let id = e.movie_at("Ran.mkv", "Ran", None);
    let p = e.edit_with(
        agent(),
        vec![set(
            id,
            &[
                ("year", json!(1985)),
                ("overview", json!("A warlord divides his realm.")),
            ],
        )],
        false,
        true,
    );
    let cs = cs_id(&p);
    assert_eq!(
        (p.done, p.changeset.as_ref().unwrap().state),
        (0, ChangesetState::Proposed)
    );
    assert!(!e.values(id).contains_key("year"), "a proposal changes nothing");
    assert_eq!(e.cat.read_blocking(changes::proposed_count).unwrap(), 1);
    // Someone writes a description meanwhile: that change is skipped.
    e.edit(
        person(),
        vec![set(id, &[("overview", json!("Kurosawa's King Lear."))])],
    );
    let applied = e
        .cat
        .writer()
        .apply_proposal(cs, "owner@example.com".into())
        .wait()
        .unwrap();
    assert_eq!((applied.done, applied.not_done.len()), (1, 1));
    assert_eq!(applied.not_done[0].1, "overview");
    let s = applied.changeset.unwrap();
    assert_eq!(
        (s.state, s.decided_by.as_deref()),
        (ChangesetState::Applied, Some("owner@example.com"))
    );
    assert_eq!(e.values(id)["year"], json!(1985));
    assert_eq!(e.values(id)["overview"], json!("Kurosawa's King Lear."));
    // The applied part records the agent as its source, and can be undone.
    assert_eq!(
        e.source(id, "year").unwrap().agent.as_deref(),
        Some("claude-code")
    );
    e.cat.writer().revert_changeset(cs, "j".into()).wait().unwrap();
    assert!(!e.values(id).contains_key("year"));

    let p2 = cs_id(&e.edit_with(agent(), vec![set(id, &[("tags", json!(["Epic"]))])], false, true));
    let r = e
        .cat
        .writer()
        .reject_proposal(p2, "owner@example.com".into())
        .wait()
        .unwrap();
    assert_eq!(r.changeset.unwrap().state, ChangesetState::Rejected);
    assert!(!e.values(id).contains_key("tags"));
    assert!(e.cat.writer().apply_proposal(p2, "x".into()).wait().is_err());
    assert_eq!(e.cat.read_blocking(changes::proposed_count).unwrap(), 0);
}

#[test]
fn clearing_hands_a_field_back_to_flux() {
    let e = env(LibraryKind::Movies);
    let id = e.movie_at("Akira.1988.mkv", "Akira", Some(1988));
    e.edit(
        agent(),
        vec![set(id, &[("title", json!("AKIRA")), ("tags", json!(["Anime"]))])],
    );
    assert!(e.source(id, "title").unwrap().locks());
    let out = e.edit(agent(), vec![clear(id, &["title", "tags", "overview"])]);
    assert_eq!(out.done, 2, "an empty overview needs no clearing");
    assert_eq!(e.detail(id).summary.title, "Akira", "the file's title");
    assert!(e.source(id, "title").is_none());
    assert!(!e.values(id).contains_key("tags"));
    // Clearing what flux had anyway, unlocked, changes nothing.
    assert!(e.edit(agent(), vec![clear(id, &["title"])]).changeset.is_none());
}

#[test]
fn edits_are_checked_and_one_item_appears_once() {
    let e = env(LibraryKind::Movies);
    let id = e.movie_at("Stalker.mkv", "Stalker", Some(1979));
    let twice = e
        .cat
        .writer()
        .commit_edit(
            agent(),
            "x".into(),
            vec![
                set(id, &[("year", json!(1979))]),
                set(id, &[("tags", json!(["Tarkovsky"]))]),
            ],
            false,
            false,
        )
        .wait();
    assert!(twice.is_err());
    let missing = e
        .cat
        .writer()
        .commit_edit(
            agent(),
            "x".into(),
            vec![set(99_999, &[("year", json!(1979))])],
            false,
            false,
        )
        .wait();
    assert!(matches!(missing, Err(flux_catalog::CatalogError::NotFound(_))));
    // A dry run is a plan read from any connection: nothing changes.
    let plan = e
        .cat
        .read_blocking(move |c| changes::plan_edit(c, &agent(), &[set(id, &[("year", json!(1980))])], false))
        .unwrap();
    assert_eq!(plan[0].changes[0].before, Some(json!(1979)));
    assert_eq!(e.values(id)["year"], json!(1979));
}

#[test]
fn a_reparse_keeps_a_title_an_agent_set() {
    // An older parser kept the dots; a re-parse retitles what was titled
    // from the file name, but not what an agent titled.
    let e = env(LibraryKind::Movies);
    let kept_path = "Kept Film (2001)/Kept.Film.2001.mkv";
    let plain_path = "Plain Film (2002)/Plain.Film.2002.mkv";
    e.apply(vec![
        upsert(kept_path, movie("Kept.Film", Some(2001)), None),
        upsert(plain_path, movie("Plain.Film", Some(2002)), None),
    ]);
    let kept = e.find_top("Kept.Film").id;
    let plain = e.find_top("Plain.Film").id;
    e.edit(agent(), vec![set(kept, &[("title", json!("Kept By Agent"))])]);
    e.apply(vec![
        upsert(kept_path, movie("Kept Film", Some(2001)), None),
        upsert(plain_path, movie("Plain Film", Some(2002)), None),
    ]);
    assert_eq!(
        e.detail(plain).summary.title,
        "Plain Film",
        "the control is retitled"
    );
    assert_eq!(e.detail(kept).summary.title, "Kept By Agent");
}

#[test]
fn a_merge_carries_what_an_agent_set_and_its_history() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert("The Wire/S01E01.mkv", episode("The Wire", None, 1, &[1]), None),
        upsert(
            "The Wire (2002)/S01E02.mkv",
            episode("The Wire", Some(2002), 1, &[2]),
            None,
        ),
    ]);
    let tops = e.top();
    assert_eq!(tops.len(), 2, "two show folders, two shows until ids agree");
    let (a, b) = (tops[0].id, tops[1].id);
    let cs = cs_id(&e.edit(
        agent(),
        vec![set(a, &[("tags", json!(["HBO"])), ("countries", json!(["US"]))])],
    ));
    let into = a.min(b);
    let from = a.max(b);
    e.cat.writer().merge_items(from, into).wait().unwrap();
    let v = e.values(into);
    assert_eq!(v["tags"], json!(["HBO"]));
    assert_eq!(
        e.source(into, "tags").unwrap().agent.as_deref(),
        Some("claude-code")
    );
    // The changeset now names the item that stayed, so it still undoes.
    let undone = e.cat.writer().revert_changeset(cs, "j".into()).wait().unwrap();
    assert_eq!(undone.done, 2);
    assert!(!e.values(into).contains_key("tags"));
}

#[test]
fn gaps_and_find_say_what_is_missing() {
    let e = env(LibraryKind::Movies);
    let a = e.movie_at("Gisaengchung.2019.mkv", "Gisaengchung", Some(2019));
    let b = e.movie_at("Untitled Home Movie.mkv", "Untitled Home Movie", None);
    let c_ = e.movie_at("Stalker.1979.mkv", "Stalker", Some(1979));
    e.edit(
        agent(),
        vec![set(
            c_,
            &[("title", json!("Stalker")), ("ids.wikidata", json!("Q470613"))],
        )],
    );
    let g = e.cat.read_blocking(|c| agent_read::gaps(c, None)).unwrap();
    assert_eq!(g.len(), 1);
    let g = &g[0];
    assert_eq!(g.items, 3);
    assert_eq!(g.gaps.identity, Some(2), "the agent identified Stalker");
    assert_eq!(g.gaps.year, Some(1));
    assert_eq!(g.gaps.overview, Some(3));
    assert_eq!(g.gaps.poster, Some(3), "no art at all");
    assert_eq!(g.gaps.offline, Some(0));

    let find = |q: agent_read::FindQuery| {
        e.cat
            .read_blocking(move |c| agent_read::find(c, &q, &["tmdb".to_string()]))
            .unwrap()
    };
    let (items, next, total) = find(agent_read::FindQuery {
        gap: Some(Gap::Identity),
        limit: 50,
        ..Default::default()
    });
    assert_eq!((total, next), (2, None));
    assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), [a, b]);
    assert!(items[1].gaps.contains(&Gap::Year));
    assert_eq!(items[0].path.as_deref(), Some("/media/lib/Gisaengchung.2019.mkv"));
    // Paged.
    let (first, next, total) = find(agent_read::FindQuery {
        limit: 2,
        ..Default::default()
    });
    assert_eq!((first.len(), total), (2, 3));
    let (rest, end, _) = find(agent_read::FindQuery {
        limit: 2,
        after: next,
        ..Default::default()
    });
    assert_eq!((rest.len(), end), (1, None));
    // Text in the title or the path.
    let (hit, _, _) = find(agent_read::FindQuery {
        q: Some("home mov".into()),
        limit: 50,
        ..Default::default()
    });
    assert_eq!(hit.iter().map(|i| i.id).collect::<Vec<_>>(), [b]);
    let (hit, _, _) = find(agent_read::FindQuery {
        q: Some("1979.mkv".into()),
        limit: 50,
        ..Default::default()
    });
    assert_eq!(hit.iter().map(|i| i.id).collect::<Vec<_>>(), [c_]);
}

#[test]
fn what_tmdb_gave_is_withheld_from_agents_but_not_from_people() {
    let e = env(LibraryKind::Movies);
    let id = e.movie_at("Gisaengchung.2019.mkv", "Gisaengchung", Some(2019));
    e.cat
        .writer()
        .update_item_metadata(
            id,
            flux_catalog::ItemMetadata {
                title: Some("Parasite".into()),
                overview: Some("TMDB's description.".into()),
                genres: Some(vec!["Thriller".into()]),
                tmdb_id: Some(496243),
                matched: true,
                provider: Some("tmdb".into()),
                match_evidence: Some(vec!["best candidate: TMDB 496243 \"Parasite\" (2019)".into()]),
                ..Default::default()
            },
        )
        .wait()
        .unwrap();
    let hide = vec!["tmdb".to_string()];
    let d = e
        .cat
        .read_blocking({
            let hide = hide.clone();
            move |c| agent_read::item(c, id, &hide)
        })
        .unwrap()
        .unwrap();
    assert_eq!(d.item.title, "Gisaengchung", "the file's title stands in");
    assert_eq!(d.fields.title.as_deref(), Some("Gisaengchung"));
    assert_eq!(d.fields.overview, None);
    assert_eq!(d.fields.genres, None);
    assert_eq!(d.withheld.get("overview").map(String::as_str), Some("tmdb"));
    assert_eq!(d.withheld.get("title").map(String::as_str), Some("tmdb"));
    assert_eq!(d.sources["title"].source, flux_core::agent::Source::File);
    assert_eq!(
        d.fields.ids.as_ref().unwrap()["tmdb"],
        "496243",
        "ids stay visible"
    );
    assert_eq!(
        d.files[0].parsed.as_ref().unwrap().title.as_deref(),
        Some("Gisaengchung")
    );
    // Searching TMDB's title finds nothing for an agent; the path still does.
    let (hits, _, _) = e
        .cat
        .read_blocking({
            let hide = hide.clone();
            move |c| {
                agent_read::find(
                    c,
                    &agent_read::FindQuery {
                        q: Some("parasite".into()),
                        limit: 10,
                        ..Default::default()
                    },
                    &hide,
                )
            }
        })
        .unwrap();
    assert!(hits.is_empty());
    // A person sees it all.
    let p = e
        .cat
        .read_blocking(move |c| agent_read::item(c, id, &[]))
        .unwrap()
        .unwrap();
    assert_eq!(p.item.title, "Parasite");
    assert_eq!(p.fields.overview.as_deref(), Some("TMDB's description."));
    assert!(p.withheld.is_empty());
    assert_eq!(p.sources["overview"].provider.as_deref(), Some("tmdb"));
    // Changes show an agent no TMDB value either.
    let cs = cs_id(&e.edit(
        agent(),
        vec![set(
            id,
            &[(
                "overview",
                json!("A poor family schemes its way into a rich household."),
            )],
        )],
    ));
    let (_, rows) = e
        .cat
        .read_blocking(move |c| changes::get(c, cs))
        .unwrap()
        .unwrap();
    assert_eq!(rows[0].before_provider.as_deref(), Some("tmdb"));
    assert_eq!(rows[0].title_provider.as_deref(), Some("tmdb"));
}

#[test]
fn chosen_art_shows_first_and_its_undo_removes_it() {
    let e = env(LibraryKind::Movies);
    let id = e.movie_at("Paprika.mkv", "Paprika", Some(2006));
    let w = e.cat.writer();
    let frame = w
        .add_art(
            id,
            flux_catalog::NewArt {
                kind: flux_catalog::ArtKind::Poster,
                source: ArtSource::Frame,
                path: Some("art/1-poster-0a0a0a0a.jpg".into()),
                url: None,
                width: Some(600),
                height: Some(900),
                blurhash: None,
                attribution: None,
                generator: None,
                gen_version: Some(1),
                gen_title: None,
            },
        )
        .wait()
        .unwrap();
    let chosen = flux_catalog::NewArt {
        kind: flux_catalog::ArtKind::Poster,
        source: ArtSource::Custom,
        path: Some("art/1-customposter-0b0b0b0b.jpg".into()),
        url: None,
        width: Some(1000),
        height: Some(1500),
        blurhash: None,
        attribution: Some("Sony Pictures Classics".into()),
        generator: None,
        gen_version: None,
        gen_title: None,
    };
    let (art, out) = w
        .add_chosen_art(
            agent(),
            "Added a poster".into(),
            id,
            chosen,
            Some("https://example.org/press".into()),
            true,
            vec![],
        )
        .wait()
        .unwrap();
    let best = e
        .cat
        .read_blocking(move |c| read::best_art(c, id, flux_catalog::ArtKind::Poster))
        .unwrap()
        .unwrap();
    assert_eq!(best.url, format!("/api/v1/art/{art}"));
    assert_eq!(best.source, ArtSource::Custom);
    let row = e.cat.read_blocking(move |c| read::art(c, art)).unwrap().unwrap();
    assert_eq!(row.found_at.as_deref(), Some("https://example.org/press"));
    assert!(row.pinned);
    // Pin the frame instead, then undo that: the chosen one shows again.
    let pin = w
        .choose_art(
            person(),
            "Frame".into(),
            id,
            flux_catalog::ArtKind::Poster,
            Some(frame),
        )
        .wait()
        .unwrap();
    w.revert_changeset(cs_id(&pin), "j".into()).wait().unwrap();
    let best = e
        .cat
        .read_blocking(move |c| read::best_art(c, id, flux_catalog::ArtKind::Poster))
        .unwrap()
        .unwrap();
    assert_eq!(best.url, format!("/api/v1/art/{art}"));
    // Undo the addition: the image goes, and its file is the caller's to delete.
    let undone = w.revert_changeset(cs_id(&out), "j".into()).wait().unwrap();
    assert_eq!(
        undone.art_removed,
        [(art, Some("art/1-customposter-0b0b0b0b.jpg".to_string()))]
    );
    assert!(e.cat.read_blocking(move |c| read::art(c, art)).unwrap().is_none());
    let best = e
        .cat
        .read_blocking(move |c| read::best_art(c, id, flux_catalog::ArtKind::Poster))
        .unwrap()
        .unwrap();
    assert_eq!(best.source, ArtSource::Frame);
    // An image of another item can't be chosen.
    let other = e.movie_at("Perfect Blue.mkv", "Perfect Blue", Some(1997));
    assert!(w
        .choose_art(
            person(),
            "x".into(),
            other,
            flux_catalog::ArtKind::Poster,
            Some(frame)
        )
        .wait()
        .is_err());
}

#[test]
fn lenses_are_kept_per_person_and_for_everyone() {
    let e = env(LibraryKind::Movies);
    let spec = |name: &str| flux_core::agent::LensSpec {
        name: name.into(),
        blurb: "b".into(),
        input: flux_core::agent::LensInput::X,
        step: 160.0,
        window: 40,
        animated: false,
        css: ".lens{}".into(),
        scene: String::new(),
        item: "<div class=\"art\"></div>".into(),
        layout: "function layout(items, view) { return items.map(() => ({})); }".into(),
    };
    let w = e.cat.writer();
    w.put_lens(
        1,
        "s-board".into(),
        spec("Board"),
        "a@x".into(),
        Some("claude-code".into()),
    )
    .wait()
    .unwrap();
    w.put_lens(0, "s-shared".into(), spec("Shared"), "a@x".into(), None)
        .wait()
        .unwrap();
    w.put_lens(2, "s-other".into(), spec("Other"), "b@x".into(), None)
        .wait()
        .unwrap();
    let mine = e.cat.read_blocking(|c| flux_catalog::lenses::list(c, 1)).unwrap();
    assert_eq!(
        mine.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
        ["s-board", "s-shared"]
    );
    assert!(mine[1].everyone && !mine[0].everyone);
    assert_eq!(mine[0].agent.as_deref(), Some("claude-code"));
    // The same id replaces.
    let again = w
        .put_lens(1, "s-board".into(), spec("Board 2"), "a@x".into(), None)
        .wait()
        .unwrap();
    assert_eq!(again.spec.name, "Board 2");
    assert_eq!(again.added_at, mine[0].added_at);
    for i in 0..23 {
        w.put_lens(1, format!("s-{i}"), spec("n"), "a@x".into(), None)
            .wait()
            .unwrap();
    }
    assert!(
        w.put_lens(1, "s-25".into(), spec("n"), "a@x".into(), None)
            .wait()
            .is_err(),
        "24 at most"
    );
    assert!(w.remove_lens(1, "s-board".into()).wait().unwrap());
    assert!(!w.remove_lens(1, "s-board".into()).wait().unwrap());
}
