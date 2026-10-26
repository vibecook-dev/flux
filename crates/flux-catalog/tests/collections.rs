//! Collections kept on the server (docs/AGENT-NATIVE.md): titles picked in
//! order, a rule that takes matching titles as the library changes (never
//! by a value the reader may not see), and pages of them as the app shows
//! a space.

mod common;

use common::*;
use flux_catalog::changes::{Actor, ItemEdit};
use flux_catalog::collections::{self, Put, EVERYONE};
use flux_catalog::{read, ItemsQuery};
use flux_core::agent::{CollectionPick, CollectionRule};
use flux_core::api::Id;
use flux_core::LibraryKind;
use serde_json::json;

fn pick(item: Id, note: Option<&str>) -> CollectionPick {
    CollectionPick {
        item,
        note: note.map(str::to_string),
    }
}

fn put(name: &str) -> Put {
    Put {
        id: None,
        owner: EVERYONE,
        name: name.into(),
        description: None,
        items: None,
        add: Vec::new(),
        remove: Vec::new(),
        rule: None,
        sources: Vec::new(),
        by: "james@example.com".into(),
        agent: Some("claude-code".into()),
    }
}

impl Env {
    fn keep(&self, p: Put) -> collections::Row {
        self.cat.writer().put_collection(p).wait().unwrap()
    }

    fn members(&self, r: &collections::Row, hide: &[&str]) -> Vec<String> {
        let r = r.clone();
        let hide: Vec<String> = hide.iter().map(|s| s.to_string()).collect();
        self.cat
            .read_blocking(move |c| {
                let ids = collections::members(c, &r, &hide)?;
                ids.into_iter()
                    .map(|id| Ok(c.query_row("SELECT title FROM item WHERE id = ?1", [id], |x| x.get(0))?))
                    .collect()
            })
            .unwrap()
    }

    fn set_genres(&self, item: Id, genres: serde_json::Value) {
        self.cat
            .writer()
            .commit_edit(
                Actor {
                    login: "j".into(),
                    agent: Some("claude-code".into()),
                },
                "genres".into(),
                vec![ItemEdit {
                    item,
                    set: [("genres".to_string(), genres)].into(),
                    ..ItemEdit::default()
                }],
                false,
                false,
            )
            .wait()
            .unwrap();
    }
}

fn library() -> (Env, Id, Id, Id) {
    let e = env(LibraryKind::Movies);
    e.apply(vec![
        upsert(
            "Spirited Away (2001).mkv",
            movie("Spirited Away", Some(2001)),
            None,
        ),
        upsert("Totoro (1988).mkv", movie("Totoro", Some(1988)), None),
        upsert("Heat (1995).mkv", movie("Heat", Some(1995)), None),
    ]);
    let (a, b, c) = (
        e.find_top("Spirited Away").id,
        e.find_top("Totoro").id,
        e.find_top("Heat").id,
    );
    (e, a, b, c)
}

#[test]
fn picked_titles_keep_their_order_and_notes() {
    let (e, spirited, totoro, _) = library();
    let r = e.keep(Put {
        items: Some(vec![pick(totoro, Some("Where it began")), pick(spirited, None)]),
        ..put("Ghibli")
    });
    assert_eq!(e.members(&r, &[]), ["Totoro", "Spirited Away"]);
    let info = e
        .cat
        .read_blocking({
            let r = r.clone();
            move |c| collections::info(c, &r, &[])
        })
        .unwrap();
    assert_eq!((info.count, info.cover.as_slice()), (2, &[totoro, spirited][..]));
    // The same name again changes it: one taken out, the order stays.
    let r2 = e.keep(Put {
        remove: vec![totoro],
        ..put("ghibli")
    });
    assert_eq!(r2.id, r.id);
    assert_eq!(e.members(&r2, &[]), ["Spirited Away"]);
    // A season or an episode isn't a title for a collection.
    let err = e
        .cat
        .writer()
        .put_collection(Put {
            add: vec![pick(9999, None)],
            ..put("Bad")
        })
        .wait()
        .unwrap_err();
    assert!(err.to_string().contains("9999"), "{err}");
}

#[test]
fn a_rule_takes_matching_titles_as_the_library_changes() {
    let (e, spirited, totoro, heat) = library();
    e.set_genres(spirited, json!(["Animation", "Fantasy"]));
    let r = e.keep(Put {
        rule: Some(CollectionRule {
            genres: vec!["animation".into()],
            ..CollectionRule::default()
        }),
        items: Some(vec![pick(heat, Some("Not animated, but loved"))]),
        ..put("Animated")
    });
    assert_eq!(
        e.members(&r, &[]),
        ["Heat", "Spirited Away"],
        "picked first, then the rule's"
    );
    e.set_genres(totoro, json!(["Animation"]));
    assert_eq!(e.members(&r, &[]), ["Heat", "Spirited Away", "Totoro"]);
    // Years and text.
    let r = e.keep(Put {
        rule: Some(CollectionRule {
            year_from: Some(1990),
            text: Some("a".into()),
            ..CollectionRule::default()
        }),
        ..put("Since 1990")
    });
    assert_eq!(e.members(&r, &[]), ["Heat", "Spirited Away"]);
}

#[test]
fn a_rule_never_counts_what_the_reader_may_not_see() {
    let (e, spirited, _, _) = library();
    // A provider (TMDB) gave Spirited Away its genres.
    e.cat
        .writer()
        .update_item_metadata(
            spirited,
            flux_catalog::ItemMetadata {
                genres: Some(vec!["Animation".into()]),
                tmdb_id: Some(129),
                provider: Some("tmdb".into()),
                ..Default::default()
            },
        )
        .wait()
        .unwrap();
    let r = e.keep(Put {
        rule: Some(CollectionRule {
            genres: vec!["Animation".into()],
            ..CollectionRule::default()
        }),
        ..put("Animated")
    });
    assert_eq!(e.members(&r, &[]), ["Spirited Away"], "a person sees it");
    assert!(
        e.members(&r, &["tmdb"]).is_empty(),
        "an agent doesn't learn TMDB's genres"
    );
}

#[test]
fn a_space_pages_a_collection_like_a_library_and_merges_keep_titles_in() {
    let (e, spirited, totoro, heat) = library();
    let r = e.keep(Put {
        items: Some(vec![pick(spirited, None), pick(totoro, None)]),
        ..put("Ghibli")
    });
    let cid = r.id;
    let page = e
        .cat
        .read_blocking(move |c| {
            read::items_page(
                c,
                &ItemsQuery {
                    collection: Some(cid),
                    sort: flux_core::api::ItemSort::Year,
                    limit: 1,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    assert_eq!(page.total, Some(2));
    assert_eq!(page.items[0].title, "Totoro");
    let cur = page.next.clone().unwrap();
    let page2 = e
        .cat
        .read_blocking(move |c| {
            read::items_page(
                c,
                &ItemsQuery {
                    collection: Some(cid),
                    sort: flux_core::api::ItemSort::Year,
                    limit: 1,
                    cursor: Some(cur),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    assert_eq!(page2.items[0].title, "Spirited Away");
    // Heat merged into Totoro (say): the collection keeps the title that stays.
    let edits = e
        .cat
        .read_blocking(move |c| flux_catalog::fixes::merge_edits(c, heat, &[totoro], &[]))
        .unwrap();
    e.cat
        .writer()
        .commit_fixes(
            Actor {
                login: "j".into(),
                agent: None,
            },
            "merge".into(),
            edits,
            false,
            false,
            Vec::new(),
        )
        .wait()
        .unwrap();
    assert_eq!(e.members(&r, &[]), ["Spirited Away", "Heat"]);
    assert!(e.cat.writer().remove_collection(EVERYONE, cid).wait().unwrap());
    assert!(e
        .cat
        .read_blocking(move |c| collections::get(c, cid))
        .unwrap()
        .is_none());
}
