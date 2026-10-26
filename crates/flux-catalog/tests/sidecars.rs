//! Metadata files beside the videos (docs/AGENT-NATIVE.md, "Sidecars"):
//! `.flux.json` and `.nfo` facts, how they rank, what's withheld from
//! agents, and a `.flux.json` that says what its video is.

mod common;

use common::*;
use flux_catalog::changes::{Actor, ItemEdit};
use flux_catalog::metafiles::{self, MetaFileRow};
use flux_catalog::{agent_read, fields};
use flux_core::api::Id;
use flux_core::LibraryKind;
use serde_json::json;

impl Env {
    fn meta(&self, rel: &str, format: &str, body: &str) -> flux_catalog::ApplyOutcome {
        let row = MetaFileRow {
            rel_path: rel.into(),
            size: body.len() as u64,
            mtime_ns: 1,
            format: format.into(),
            content: metafiles::parse(format, body.as_bytes()).ok(),
            error: None,
        };
        self.cat
            .writer()
            .sync_meta_files(self.root, vec![row], Vec::new())
            .wait()
            .unwrap()
    }

    fn gone(&self, rel: &str) {
        self.cat
            .writer()
            .sync_meta_files(self.root, Vec::new(), vec![rel.to_string()])
            .wait()
            .unwrap();
    }

    fn value(&self, id: Id, field: &'static str) -> Option<serde_json::Value> {
        self.cat
            .read_blocking(move |c| Ok(fields::read(c, id)?.unwrap().values.get(field).cloned()))
            .unwrap()
    }

    fn src(&self, id: Id, field: &'static str) -> Option<String> {
        self.cat
            .read_blocking(move |c| fields::source_of(c, id, field))
            .unwrap()
            .map(|r| format!("{}:{}", r.source, r.provider.unwrap_or_default()))
    }
}

const NFO: &str =
    "<movie><title>Spirited Away</title><year>2001</year><plot>A girl in a spirit world.</plot>\
    <uniqueid type=\"imdb\">tt0245429</uniqueid><director>Hayao Miyazaki</director></movie>";

#[test]
fn an_nfo_fills_in_and_ranks_below_people_and_agents() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert(
        "Sen to Chihiro/Sen to Chihiro.mkv",
        movie("Sen to Chihiro", None),
        None,
    )]);
    let id = e.find_top("Sen to Chihiro").id;
    let out = e.meta("Sen to Chihiro/Sen to Chihiro.nfo", "nfo", NFO);
    assert!(out.items_updated.contains(&id), "{out:?}");
    assert_eq!(e.value(id, "title"), Some(json!("Spirited Away")));
    assert_eq!(e.value(id, "year"), Some(json!(2001)));
    assert_eq!(e.value(id, "directors"), Some(json!(["Hayao Miyazaki"])));
    assert_eq!(e.src(id, "overview").as_deref(), Some("sidecar:nfo"));

    // An agent's own description wins, and a rescan of the NFO leaves it.
    e.cat
        .writer()
        .commit_edit(
            Actor {
                login: "j".into(),
                agent: Some("claude-code".into()),
            },
            "mine".into(),
            vec![ItemEdit {
                item: id,
                set: [("overview".to_string(), json!("My words."))].into(),
                ..ItemEdit::default()
            }],
            false,
            false,
        )
        .wait()
        .unwrap();
    e.meta("Sen to Chihiro/Sen to Chihiro.nfo", "nfo", NFO);
    assert_eq!(e.value(id, "overview"), Some(json!("My words.")));

    // An agent sees the NFO's text as withheld (it's usually TMDB's), and its ids.
    let hide = vec!["nfo".to_string()];
    let d = e
        .cat
        .read_blocking(move |c| agent_read::item(c, id, &hide))
        .unwrap()
        .unwrap();
    assert_eq!(d.withheld.get("directors").map(String::as_str), Some("nfo"));
    assert!(d.fields.directors.is_none());
    assert_eq!(d.fields.overview.as_deref(), Some("My words."));
    assert_eq!(d.item.title, "Sen to Chihiro", "the file's title stands in");
    assert_eq!(d.fields.ids.as_ref().unwrap()["imdb"], "tt0245429");
    assert_eq!(d.files[0].sidecars, ["Sen to Chihiro/Sen to Chihiro.nfo"]);

    // The NFO goes: what it gave is flux's again (the values stay).
    e.gone("Sen to Chihiro/Sen to Chihiro.nfo");
    assert_eq!(e.src(id, "directors"), None);
    assert_eq!(e.value(id, "directors"), Some(json!(["Hayao Miyazaki"])));
    assert_eq!(e.src(id, "overview").as_deref(), Some("agent:"));
}

#[test]
fn a_flux_sidecar_says_what_its_video_is_and_wins_over_an_nfo() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert("星际穿越4k.mp4", movie("星际穿越", None), None)]);
    e.meta(
        "星际穿越4k.nfo",
        "nfo",
        "<movie><title>Interstellar (NFO)</title><tagline>From the NFO</tagline></movie>",
    );
    e.meta(
        "星际穿越4k.flux.json",
        "flux",
        r#"{"flux": "flux.sidecar/1", "fields": {"title": "Interstellar", "year": 2014},
            "fix": {"title": "Interstellar", "year": 2014}, "by": "claude-code",
            "sources": ["https://en.wikipedia.org/wiki/Interstellar_(film)"]}"#,
    );
    let top = e.top();
    assert_eq!(top.len(), 1, "{top:?}");
    let id = top[0].id;
    assert_eq!(top[0].title, "Interstellar");
    assert_eq!(top[0].year, Some(2014));
    assert_eq!(e.src(id, "title").as_deref(), Some("sidecar:flux"));
    assert_eq!(
        e.value(id, "tagline"),
        Some(json!("From the NFO")),
        "what flux.json doesn't say"
    );
    let file = e.file_id("星际穿越4k.mp4");
    let fix = e
        .cat
        .read_blocking(move |c| flux_catalog::fixes::get(c, file))
        .unwrap()
        .unwrap();
    assert_eq!((fix.0.year, fix.1.source.as_str()), (Some(2014), "sidecar"));
    // The sidecar goes: its fix too.
    e.gone("星际穿越4k.flux.json");
    assert!(e
        .cat
        .read_blocking(move |c| flux_catalog::fixes::get(c, file))
        .unwrap()
        .is_none());
}

#[test]
fn a_video_found_after_its_sidecar_takes_what_it_says() {
    let e = env(LibraryKind::Movies);
    e.meta(
        "Heat (1995)/Heat (1995).nfo",
        "nfo",
        "<movie><title>Heat</title><director>Michael Mann</director></movie>",
    );
    e.apply(vec![upsert(
        "Heat (1995)/Heat (1995).mkv",
        movie("Heat", Some(1995)),
        None,
    )]);
    let id = e.find_top("Heat").id;
    assert_eq!(e.value(id, "directors"), Some(json!(["Michael Mann"])));
}

#[test]
fn a_shows_flux_json_names_its_episodes() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![
        upsert(
            "拾荒者统治/Season 1/拾荒者统治 S01E01.mkv",
            episode("拾荒者统治", None, 1, &[1]),
            None,
        ),
        upsert(
            "拾荒者统治/Season 1/拾荒者统治 S01E02.mkv",
            episode("拾荒者统治", None, 1, &[2]),
            None,
        ),
    ]);
    e.meta(
        "拾荒者统治/flux.json",
        "flux",
        r#"{"flux": "flux.sidecar/1", "fields": {"title": "Scavengers Reign", "year": 2023},
            "episodes": [{"season": 1, "episode": 1, "fields": {"title": "The Signal"}},
                         {"season": 1, "episode": 2, "fields": {"title": "The Nest"}}]}"#,
    );
    let show = e.find_top("Scavengers Reign").id;
    let eps: Vec<String> = e
        .children(e.children(show)[0].id)
        .into_iter()
        .map(|i| i.title)
        .collect();
    assert_eq!(eps, ["The Signal", "The Nest"]);
}
