//! flux for agents (docs/AGENT-NATIVE.md) through the router: the
//! manifest and schemas, operations over HTTP (roles, errors, changesets,
//! what's withheld), MCP, lenses and art. Catalogs are seeded with
//! synthetic files, so no media corpus is needed.

mod common;

use std::time::Instant;

use axum::http::{Method, StatusCode};
use base64::Engine as _;
use common::*;
use flux_catalog::{scan_support, FileFacts, FileUpsert, ScanBatch};
use flux_core::agent::{
    AgentItemDetail, Changeset, ChangesetState, EditOutput, FindOutput, GapsOutput, Manifest, Status,
};
use flux_core::api::{CreateLibrary, Id, Identity, Role};
use flux_core::{GuessKind, LibraryKind, NameGuess};
use flux_server::{api::router, build_state, Config};
use serde_json::{json, Value};

fn person(role: Role, login: &str) -> Identity {
    Identity {
        login_name: login.into(),
        display_name: login.into(),
        profile_pic_url: None,
        node_name: None,
        role,
        tags: Vec::new(),
    }
}

fn movie(title: &str, year: Option<u16>) -> NameGuess {
    let mut g = NameGuess::unknown();
    g.kind = GuessKind::Movie;
    g.title = Some(title.into());
    g.year = year;
    g.confidence = 0.9;
    g
}

/// A movies library with these files (path, title, year).
fn seed_movies(st: &Shared, files: &[(&str, &str, Option<u16>)]) -> Id {
    let cat = st.catalog.clone();
    let lib = cat
        .writer()
        .create_library(CreateLibrary {
            name: "Films".into(),
            kind: LibraryKind::Movies,
            roots: vec!["/media/films".into()],
        })
        .wait()
        .unwrap();
    let root = cat
        .read_blocking(|c| Ok(scan_support::list_roots(c)?[0].id))
        .unwrap();
    let upserts = files
        .iter()
        .enumerate()
        .map(|(i, (path, title, year))| FileUpsert {
            facts: FileFacts {
                rel_path: (*path).into(),
                dev: Some(1),
                ino: Some(100 + i as u64),
                btime_ns: None,
                size: 1_000_000 + i as u64,
                mtime_ns: 1_700_000_000_000_000_000,
                fp128: Some(1 + i as u128),
                oshash: None,
            },
            probe: Default::default(),
            guess: Some(movie(title, *year)),
        })
        .collect();
    cat.writer()
        .apply(ScanBatch {
            root_id: root,
            upserts,
            ..Default::default()
        })
        .wait()
        .unwrap();
    lib.id
}

async fn setup(files: &[(&str, &str, Option<u16>)]) -> (tempfile::TempDir, Shared, axum::Router) {
    let dir = scratch("agents");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let st2 = st.clone();
    let files: Vec<(String, String, Option<u16>)> = files
        .iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), *c))
        .collect();
    tokio::task::block_in_place(move || {
        let f: Vec<(&str, &str, Option<u16>)> = files
            .iter()
            .map(|(a, b, c)| (a.as_str(), b.as_str(), *c))
            .collect();
        seed_movies(&st2, &f);
    });
    let app = router(st.clone());
    (dir, st, app)
}

/// Run an operation as `who`, as `agent`.
async fn op(
    app: &axum::Router,
    who: Option<Identity>,
    agent: Option<&str>,
    name: &str,
    input: Value,
) -> Resp {
    let headers: Vec<(&str, &str)> = agent.map(|a| vec![("x-flux-agent", a)]).unwrap_or_default();
    call(
        app,
        Method::POST,
        &format!("/api/v1/ops/{name}"),
        who,
        &headers,
        Some(input),
    )
    .await
}

async fn owner_op(app: &axum::Router, name: &str, input: Value) -> Value {
    let r = op(app, Some(owner()), Some("claude-code"), name, input).await;
    assert_eq!(r.status, StatusCode::OK, "{name}: {}", r.text());
    r.json()
}

fn id_of(found: &FindOutput, title: &str) -> Id {
    found
        .items
        .iter()
        .find(|i| i.title == title)
        .unwrap_or_else(|| panic!("no {title:?} in {found:?}"))
        .id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_manifest_lists_operations_schemas_and_skills() {
    let (_d, _st, app) = setup(&[]).await;
    let m: Manifest = get(&app, "/api/v1/agent").await.json();
    assert_eq!(m.contract, "flux.agent/1");
    assert_eq!(m.mcp, "/mcp");
    let names: Vec<&str> = m.ops.iter().map(|o| o.name.as_str()).collect();
    for n in [
        "status",
        "library.gaps",
        "items.find",
        "item.get",
        "items.edit",
        "art.add",
        "changes.undo",
        "lens.add",
    ] {
        assert!(names.contains(&n), "{n} in {names:?}");
    }
    assert_eq!(m.policy.withheld, ["tmdb"]);
    assert!(m.skills.iter().any(|s| s.name == "flux-metadata"));
    // Schemas are JSON Schema 2020-12, served by name.
    let s = get(&app, "/api/v1/schemas/items.edit.input").await;
    assert_eq!(s.status, StatusCode::OK);
    assert_eq!(
        s.header("content-type").as_deref(),
        Some("application/schema+json")
    );
    let v: Value = s.json();
    assert_eq!(v["$schema"], "https://json-schema.org/draft/2020-12/schema");
    assert!(v["properties"]["edits"].is_object());
    assert_eq!(
        get(&app, "/api/v1/schemas/nope").await.status,
        StatusCode::NOT_FOUND
    );
    let skill = get(&app, "/api/v1/agent/skills/flux").await;
    assert!(skill.text().starts_with("---\nname: flux\n"));
    assert_eq!(
        skill.header("content-type").as_deref(),
        Some("text/markdown; charset=utf-8")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_finds_gaps_edits_with_sources_and_the_owner_undoes_it() {
    let (_d, st, app) = setup(&[
        ("Gisaengchung.2019.mkv", "Gisaengchung", Some(2019)),
        ("Untitled.mkv", "Untitled", None),
    ])
    .await;
    let s: Status = serde_json::from_value(owner_op(&app, "status", json!({})).await).unwrap();
    assert_eq!(s.you.agent.as_deref(), Some("claude-code"));
    assert!(s.you.may.apply && s.you.may.admin);
    assert_eq!(s.libraries[0].items, 2);
    assert_eq!(format!("{:?}", s.metadata_source), "None");

    let g: GapsOutput = serde_json::from_value(owner_op(&app, "library.gaps", json!({})).await).unwrap();
    assert_eq!(
        (g.libraries[0].gaps.identity, g.libraries[0].gaps.year),
        (Some(2), Some(1))
    );
    let found: FindOutput =
        serde_json::from_value(owner_op(&app, "items.find", json!({"gap": "identity"})).await).unwrap();
    assert_eq!(found.total, 2);
    let id = id_of(&found, "Gisaengchung");

    let d: AgentItemDetail =
        serde_json::from_value(owner_op(&app, "item.get", json!({"item": id})).await).unwrap();
    assert_eq!(d.files[0].path, "Gisaengchung.2019.mkv");
    assert_eq!(d.files[0].root, "/media/films");
    assert!(d.files[0].fix.is_none() && d.files[0].sidecars.is_empty());
    assert_eq!(
        d.files[0].parsed.as_ref().unwrap().title.as_deref(),
        Some("Gisaengchung")
    );

    // A dry run first: nothing changes.
    let edit = json!({
        "summary": "Identified Gisaengchung as Parasite",
        "edits": [{
            "item": id,
            "set": {"title": "Parasite", "original_title": "기생충", "directors": ["Bong Joon-ho"], "ids": {"wikidata": "Q61448040"}},
            "sources": [{"url": "https://en.wikipedia.org/wiki/Parasite_(2019_film)", "title": "Wikipedia"}]
        }],
        "dry_run": true
    });
    let dry: EditOutput = serde_json::from_value(owner_op(&app, "items.edit", edit.clone()).await).unwrap();
    assert!(dry.changeset.is_none());
    assert_eq!(dry.items[0].changed.len(), 4);
    let mut events = st.events.subscribe();
    let mut real = edit.clone();
    real["dry_run"] = json!(false);
    let out: EditOutput = serde_json::from_value(owner_op(&app, "items.edit", real).await).unwrap();
    let cs = out.changeset.unwrap();
    assert_eq!(
        (cs.state, cs.agent.as_deref(), cs.by.as_str()),
        (ChangesetState::Applied, Some("claude-code"), "owner@example.com")
    );
    // The app hears about it.
    let mut kinds = Vec::new();
    while let Ok(e) = events.try_recv() {
        kinds.push(
            serde_json::to_value(&e).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert!(
        kinds.contains(&"changes".to_string()) && kinds.contains(&"items_changed".to_string()),
        "{kinds:?}"
    );

    let d: AgentItemDetail =
        serde_json::from_value(owner_op(&app, "item.get", json!({"item": id})).await).unwrap();
    assert_eq!(d.item.title, "Parasite");
    assert_eq!(
        d.fields.directors.as_deref(),
        Some(&["Bong Joon-ho".to_string()][..])
    );
    let src = &d.sources["title"];
    assert_eq!(
        (src.agent.as_deref(), src.changeset),
        (Some("claude-code"), Some(cs.id))
    );
    assert_eq!(
        src.sources[0].url,
        "https://en.wikipedia.org/wiki/Parasite_(2019_film)"
    );
    assert!(!d.item.gaps.contains(&flux_core::agent::Gap::Identity));
    assert_eq!(d.changes[0].id, cs.id);
    // The web app sees the new title too.
    let detail: flux_core::api::ItemDetail = get(&app, &format!("/api/v1/items/{id}")).await.json();
    assert_eq!(detail.summary.title, "Parasite");

    let c: Changeset =
        serde_json::from_value(owner_op(&app, "changes.get", json!({"changeset": cs.id})).await).unwrap();
    assert_eq!(c.changes.len(), 4);
    assert!(c
        .changes
        .iter()
        .any(|ch| ch.field == "title" && ch.before == Some(json!("Gisaengchung"))));

    let undone = owner_op(&app, "changes.undo", json!({"changeset": cs.id})).await;
    assert_eq!(undone["done"], 4);
    assert_eq!(undone["changeset"]["state"], "reverted");
    let detail: flux_core::api::ItemDetail = get(&app, &format!("/api/v1/items/{id}")).await.json();
    assert_eq!(detail.summary.title, "Gisaengchung");
    // Twice is a conflict.
    let again = op(
        &app,
        Some(owner()),
        None,
        "changes.undo",
        json!({"changeset": cs.id}),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.json::<Value>()["error"]["code"], "conflict");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roles_decide_apply_propose_or_nothing() {
    let (_d, _st, app) = setup(&[("Heat.1995.mkv", "Heat", Some(1995))]).await;
    let found: FindOutput = serde_json::from_value(owner_op(&app, "items.find", json!({})).await).unwrap();
    let id = id_of(&found, "Heat");
    let edit =
        |tagline: &str| json!({"summary": "A tagline", "edits": [{"item": id, "set": {"tagline": tagline}}]});

    // A Viewer's agent proposes; an Owner applies it.
    let viewer = person(Role::Viewer, "kid@example.com");
    let r = op(
        &app,
        Some(viewer.clone()),
        Some("codex"),
        "items.edit",
        edit("A Los Angeles crime saga."),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let out: EditOutput = r.json();
    let cs = out.changeset.unwrap();
    assert_eq!(cs.state, ChangesetState::Proposed);
    let d: AgentItemDetail =
        serde_json::from_value(owner_op(&app, "item.get", json!({"item": id})).await).unwrap();
    assert_eq!(d.fields.tagline, None);
    let list = owner_op(&app, "changes.list", json!({"state": "proposed"})).await;
    assert_eq!(list["proposed"], 1);
    // Only an Owner or Admin decides.
    let r = op(
        &app,
        Some(viewer.clone()),
        None,
        "changes.apply",
        json!({"changeset": cs.id}),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json::<Value>()["error"]["code"], "forbidden");
    let applied = owner_op(&app, "changes.apply", json!({"changeset": cs.id})).await;
    assert_eq!(applied["changeset"]["state"], "applied");
    assert_eq!(applied["changeset"]["decided_by"], "owner@example.com");
    let d: AgentItemDetail =
        serde_json::from_value(owner_op(&app, "item.get", json!({"item": id})).await).unwrap();
    assert_eq!(d.fields.tagline.as_deref(), Some("A Los Angeles crime saga."));
    assert_eq!(d.sources["tagline"].by.as_deref(), Some("kid@example.com"));

    // Anonymous callers (the LAN) and Kids read, but change nothing.
    let r = op(&app, None, None, "items.edit", edit("x")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = op(
        &app,
        Some(person(Role::Kid, "k@x")),
        None,
        "items.edit",
        edit("x"),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = op(&app, None, None, "items.find", json!({})).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = op(&app, Some(viewer), None, "library.scan", json!({"library": 1})).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_are_json_with_a_code() {
    let (_d, _st, app) = setup(&[("Heat.1995.mkv", "Heat", Some(1995))]).await;
    let r = op(&app, Some(owner()), None, "nope.nope", json!({})).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json::<Value>()["error"]["code"], "not_found");
    // A misspelt field is refused, not ignored.
    let r = op(
        &app,
        Some(owner()),
        None,
        "items.edit",
        json!({"summary": "s", "edits": [{"item": 1, "set": {"yaer": 1995}}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let e: Value = r.json();
    assert_eq!(e["error"]["code"], "invalid");
    assert!(e["error"]["message"].as_str().unwrap().contains("yaer"), "{e}");
    let r = op(
        &app,
        Some(owner()),
        None,
        "items.edit",
        json!({"summary": "s", "edits": [{"item": 1, "set": {"year": 1500}}]}),
    )
    .await;
    assert!(r.json::<Value>()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("year"));
    let r = op(
        &app,
        Some(owner()),
        None,
        "items.edit",
        json!({"summary": "", "edits": [{"item": 1, "set": {"year": 1995}}]}),
    )
    .await;
    assert!(r.json::<Value>()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("summary"));
    let r = op(&app, Some(owner()), None, "item.get", json!({"item": 999})).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // The cross-site guard still applies (plain text, as everywhere).
    let r = call_raw(
        &app,
        Method::POST,
        "/api/v1/ops/status",
        Some(owner()),
        &[("host", "127.0.0.1:8096")],
        Some(json!({})),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tmdbs_content_is_withheld_from_agents_and_shown_to_people() {
    let (_d, st, app) = setup(&[("Gisaengchung.2019.mkv", "Gisaengchung", Some(2019))]).await;
    let found: FindOutput = serde_json::from_value(owner_op(&app, "items.find", json!({})).await).unwrap();
    let id = found.items[0].id;
    st.catalog
        .writer()
        .update_item_metadata(
            id,
            flux_catalog::ItemMetadata {
                title: Some("Parasite".into()),
                overview: Some("TMDB's own description.".into()),
                tmdb_id: Some(496243),
                matched: true,
                provider: Some("tmdb".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let agent: AgentItemDetail =
        serde_json::from_value(owner_op(&app, "item.get", json!({"item": id})).await).unwrap();
    assert_eq!(agent.item.title, "Gisaengchung");
    assert_eq!(agent.fields.overview, None);
    assert_eq!(agent.withheld["overview"], "tmdb");
    assert_eq!(agent.fields.ids.as_ref().unwrap()["tmdb"], "496243");
    let r = op(&app, Some(owner()), None, "item.get", json!({"item": id})).await;
    let person: AgentItemDetail = r.json();
    assert_eq!(person.item.title, "Parasite");
    assert_eq!(person.fields.overview.as_deref(), Some("TMDB's own description."));
    // An agent's edit replaces TMDB's value without being shown it.
    let out: EditOutput = serde_json::from_value(
        owner_op(
            &app,
            "items.edit",
            json!({"summary": "Own words", "edits": [{"item": id, "set": {"overview": "A poor family schemes its way into a wealthy household."}, "sources": ["https://en.wikipedia.org/wiki/Parasite_(2019_film)"]}]}),
        )
        .await,
    )
    .unwrap();
    let ch = &out.items[0].changed[0];
    assert_eq!((ch.before.as_ref(), ch.withheld.as_deref()), (None, Some("tmdb")));
    assert_eq!(out.items[0].title, "Gisaengchung");
}

/// One MCP message.
async fn mcp(app: &axum::Router, session: Option<&str>, msg: Value) -> Resp {
    let mut h = vec![
        ("host", "127.0.0.1:8096"),
        ("accept", "application/json, text/event-stream"),
    ];
    if let Some(s) = session {
        h.push(("mcp-session-id", s));
    }
    call_raw(app, Method::POST, "/mcp", Some(owner()), &h, Some(msg)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_serves_the_operations_as_tools() {
    let (_d, _st, app) = setup(&[("Ran.mkv", "Ran", None)]).await;
    let init = mcp(
        &app,
        None,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "2.1"}
        }}),
    )
    .await;
    assert_eq!(init.status, StatusCode::OK, "{}", init.text());
    let session = init.header("mcp-session-id").expect("a session");
    let v: Value = init.json();
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(v["result"]["serverInfo"]["name"], "flux");
    assert!(v["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("library_gaps"));
    let n = mcp(
        &app,
        Some(&session),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(n.status, StatusCode::ACCEPTED);

    let tools: Value = mcp(
        &app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await
    .json();
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"items_edit") && names.contains(&"item_get"),
        "{names:?}"
    );

    let st: Value = mcp(
        &app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "status", "arguments": {}}}),
    )
    .await
    .json();
    assert_eq!(
        st["result"]["structuredContent"]["you"]["agent"], "claude-code",
        "{st}"
    );
    let text: Value = serde_json::from_str(st["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, st["result"]["structuredContent"]);

    let found: Value = mcp(
        &app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "items_find", "arguments": {"q": "ran"}}}),
    )
    .await
    .json();
    let id = found["result"]["structuredContent"]["items"][0]["id"]
        .as_i64()
        .unwrap();
    let edit: Value = mcp(
        &app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "items_edit", "arguments": {
            "summary": "Year from Wikipedia", "edits": [{"item": id, "set": {"year": 1985}, "sources": [{"url": "https://en.wikipedia.org/wiki/Ran_(film)"}]}]
        }}}),
    )
    .await
    .json();
    assert_eq!(
        edit["result"]["structuredContent"]["changeset"]["agent"], "claude-code",
        "{edit}"
    );
    // A tool's failure is a result the model can read, not a protocol error.
    let bad: Value = mcp(
        &app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "item_get", "arguments": {"item": 999}}}),
    )
    .await
    .json();
    assert_eq!(bad["result"]["isError"], true);
    assert!(bad["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("not_found"));
    let unknown: Value = mcp(
        &app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 7, "method": "nope"}),
    )
    .await
    .json();
    assert_eq!(unknown["error"]["code"], -32601);

    let res: Value = mcp(
        &app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 8, "method": "resources/read", "params": {"uri": "flux://skills/flux-metadata"}}),
    )
    .await
    .json();
    assert!(res["result"]["contents"][0]["text"]
        .as_str()
        .unwrap()
        .contains("name: flux-metadata"));
    let p: Value = mcp(
        &app,
        Some(&session),
        json!({"jsonrpc": "2.0", "id": 9, "method": "prompts/get", "params": {"name": "fill_gaps", "arguments": {"gap": "year"}}}),
    )
    .await
    .json();
    assert!(p["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("year gap"));

    // A session this server doesn't know: initialize again.
    let gone = mcp(
        &app,
        Some("0000"),
        json!({"jsonrpc": "2.0", "id": 10, "method": "ping"}),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    // Batches answer as batches.
    let b: Value = mcp(&app, Some(&session), json!([{"jsonrpc": "2.0", "id": 11, "method": "ping"}, {"jsonrpc": "2.0", "id": 12, "method": "ping"}]))
        .await
        .json();
    assert_eq!(b.as_array().unwrap().len(), 2);
    // Not JSON from a page on another site.
    let r = call_raw(
        &app,
        Method::POST,
        "/mcp",
        Some(owner()),
        &[("host", "127.0.0.1:8096"), ("content-type", "text/plain")],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = call_raw(
        &app,
        Method::POST,
        "/mcp",
        Some(owner()),
        &[("host", "127.0.0.1:8096"), ("origin", "https://evil.example")],
        Some(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(
        call_raw(
            &app,
            Method::GET,
            "/mcp",
            Some(owner()),
            &[("host", "127.0.0.1:8096")],
            None
        )
        .await
        .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    // No OAuth here: discovery finds nothing (rather than the app's page).
    for p in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-authorization-server/mcp",
    ] {
        assert_eq!(get(&app, p).await.status, StatusCode::NOT_FOUND, "{p}");
    }
    let del = call_raw(
        &app,
        Method::DELETE,
        "/mcp",
        Some(owner()),
        &[("host", "127.0.0.1:8096"), ("mcp-session-id", &session)],
        None,
    )
    .await;
    assert_eq!(del.status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lenses_kept_on_the_server_are_checked_and_shared() {
    let (_d, st, app) = setup(&[]).await;
    let lens = json!({
        "name": "Departure board",
        "blurb": "Titles on a split-flap board.",
        "input": "y",
        "step": 90,
        "css": ".lens .item { color: var(--text); }",
        "item": "<div class=\"art\"></div><img src=\"x.png\"><b>{{title}}</b>",
        "layout": "function layout(items, view) { return items.map(it => ({ y: (it.i - view.pos) * 90 })); }"
    });
    let c = owner_op(&app, "lens.check", json!({"spec": lens})).await;
    assert_eq!(c["ok"], true);
    assert!(c["warnings"][0].as_str().unwrap().contains("<img>"));
    let mut events = st.events.subscribe();
    let added = owner_op(&app, "lens.add", json!({"spec": lens})).await;
    assert_eq!(added["lens"]["id"], "s-departure-board");
    assert_eq!(added["lens"]["agent"], "claude-code");
    assert_eq!(
        serde_json::to_value(events.try_recv().unwrap()).unwrap()["type"],
        "lenses"
    );
    let list = owner_op(&app, "lenses.list", json!({})).await;
    assert_eq!(list["lenses"].as_array().unwrap().len(), 1);
    // Someone else doesn't see a personal lens; everyone's needs an admin.
    let other = person(Role::Viewer, "v@example.com");
    let r = op(&app, Some(other.clone()), None, "lenses.list", json!({})).await;
    assert_eq!(r.json::<Value>()["lenses"].as_array().unwrap().len(), 0);
    let r = op(
        &app,
        Some(other),
        None,
        "lens.add",
        json!({"spec": lens, "everyone": true}),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let bad = op(
        &app,
        Some(owner()),
        None,
        "lens.add",
        json!({"spec": {"name": "x", "layout": "nope"}}),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let removed = owner_op(&app, "lens.remove", json!({"id": "s-departure-board"})).await;
    assert_eq!(removed["removed"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn art_an_agent_adds_is_credited_shown_and_undone() {
    let (_d, st, app) = setup(&[("Paprika.2006.mkv", "Paprika", Some(2006))]).await;
    let found: FindOutput = serde_json::from_value(owner_op(&app, "items.find", json!({})).await).unwrap();
    let id = found.items[0].id;
    let img = image::RgbImage::from_fn(400, 600, |x, y| image::Rgb([(x / 2) as u8, (y / 3) as u8, 120]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    // A found image needs its credit.
    let r = op(
        &app,
        Some(owner()),
        Some("claude-code"),
        "art.add",
        json!({"item": id, "kind": "poster", "data_base64": b64, "origin": "found"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let added = owner_op(
        &app,
        "art.add",
        json!({"item": id, "kind": "poster", "data_base64": b64, "origin": "found", "credit": "Sony Pictures Classics", "found_at": "https://example.org/paprika"}),
    )
    .await;
    let art = added["art"]["id"].as_i64().unwrap();
    assert_eq!(added["art"]["source"], "custom");
    assert_eq!(
        (added["art"]["width"].as_u64(), added["art"]["height"].as_u64()),
        (Some(400), Some(600))
    );
    let detail: flux_core::api::ItemDetail = get(&app, &format!("/api/v1/items/{id}")).await.json();
    let poster = detail.summary.poster.unwrap();
    assert_eq!(poster.url, format!("/api/v1/art/{art}"));
    assert_eq!(poster.attribution.as_deref(), Some("Sony Pictures Classics"));
    let files: Vec<_> = std::fs::read_dir(st.art.dir.clone())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    // The image itself, to look at.
    let seen = owner_op(&app, "art.view", json!({"art": art, "width": 160})).await;
    assert_eq!(seen["mime"], "image/jpeg");
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(seen["data_base64"].as_str().unwrap())
        .unwrap();
    assert!(jpeg.starts_with(&[0xFF, 0xD8]));
    // Undo removes it, file and all.
    let undone = owner_op(
        &app,
        "changes.undo",
        json!({"changeset": added["changeset"]["id"]}),
    )
    .await;
    assert_eq!(undone["done"], 1);
    let detail: flux_core::api::ItemDetail = get(&app, &format!("/api/v1/items/{id}")).await.json();
    assert!(detail.summary.poster.is_none());
    assert_eq!(std::fs::read_dir(st.art.dir.clone()).unwrap().count(), 0);
    // AI art says so.
    let r = op(
        &app,
        Some(owner()),
        None,
        "art.add",
        json!({"item": id, "kind": "poster", "data_base64": b64, "origin": "generated"}),
    )
    .await;
    assert!(r.json::<Value>()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("generator"));
    let gen = owner_op(&app, "art.add", json!({"item": id, "kind": "poster", "data_base64": b64, "origin": "generated", "generator": "gpt-image-2"})).await;
    assert_eq!(gen["art"]["source"], "generated");
    assert_eq!(gen["art"]["generator"], "gpt-image-2");
    // Fetching from this machine is refused.
    let r = op(&app, Some(owner()), None, "art.add", json!({"item": id, "kind": "poster", "url": "http://127.0.0.1:9/p.jpg", "origin": "found", "credit": "x"})).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_server_says_where_it_listens() {
    let dir = scratch("server-file");
    let server = flux_server::start(Config::for_tests(dir.path())).await.unwrap();
    let file = dir.path().join("server.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(v["url"], format!("http://127.0.0.1:{}", server.local_addr.port()));
    assert_eq!(v["pid"], std::process::id());
    server.shutdown(std::time::Duration::from_secs(1)).await;
    assert!(!file.exists());
}
