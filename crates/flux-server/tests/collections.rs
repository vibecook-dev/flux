//! Collections through the operations and the app's route: kept per
//! person or for everyone, private ones stay private, the app pages them
//! like a library, and the events say when they change.

mod common;

use std::time::Instant;

use axum::http::{Method, StatusCode};
use common::*;
use flux_core::agent::{CollectionInfo, CollectionItems, Collections, FindOutput};
use flux_core::api::{Identity, ItemSummary, Page, Role};
use flux_core::LibraryKind;
use flux_server::{api::router, build_state, Config};
use serde_json::{json, Value};

async fn op_as(app: &axum::Router, who: Option<Identity>, name: &str, input: Value) -> (StatusCode, String) {
    let r = call(
        app,
        Method::POST,
        &format!("/api/v1/ops/{name}"),
        who,
        &[("x-flux-agent", "claude-code")],
        Some(input),
    )
    .await;
    (r.status, r.text())
}

fn viewer(login: &str) -> Identity {
    Identity {
        role: Role::Viewer,
        login_name: login.into(),
        display_name: login.into(),
        ..owner()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn collections_are_kept_shared_or_private_and_paged_like_a_library() {
    let dir = scratch("collections");
    let root = dir.path().join("media");
    for f in [
        "Spirited Away (2001).mkv",
        "My Neighbor Totoro (1988).mkv",
        "Heat (1995).mkv",
    ] {
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(f), "x").unwrap();
    }
    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    let r = root.clone();
    tokio::task::block_in_place(move || {
        seed(&cat, "Films", LibraryKind::Movies, &r, |_| true);
    });
    let app = router(st.clone());
    let mut events = st.events.subscribe();

    let (_, text) = op_as(&app, Some(owner()), "items.find", json!({})).await;
    let found: FindOutput = serde_json::from_str(&text).unwrap();
    let id = |t: &str| found.items.iter().find(|i| i.title == t).unwrap().id;
    let (spirited, totoro) = (id("Spirited Away"), id("My Neighbor Totoro"));

    // Everyone's, from the owner's agent.
    let (status, text) = op_as(
        &app,
        Some(owner()),
        "collection.put",
        json!({"name": "Ghibli", "everyone": true, "description": "Studio Ghibli's films",
               "items": [{"item": totoro, "note": "Where it began"}, {"item": spirited}],
               "sources": ["https://en.wikipedia.org/wiki/Studio_Ghibli"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let ghibli: CollectionInfo = serde_json::from_str(&text).unwrap();
    assert_eq!(
        (ghibli.count, ghibli.everyone, ghibli.agent.as_deref()),
        (2, true, Some("claude-code"))
    );
    let ev = events.try_recv().unwrap();
    assert!(matches!(ev, flux_core::api::Event::Collections), "{ev:?}");

    // A viewer may keep their own, not everyone's.
    let (status, _) = op_as(
        &app,
        Some(viewer("v@example.com")),
        "collection.put",
        json!({"name": "Mine", "everyone": true, "items": [{"item": spirited}]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, text) = op_as(
        &app,
        Some(viewer("v@example.com")),
        "collection.put",
        json!({"name": "Rainy Sunday", "rule": {"text": "heat"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let mine: CollectionInfo = serde_json::from_str(&text).unwrap();
    assert_eq!(mine.count, 1);

    // Each sees everyone's and their own.
    let (_, text) = op_as(&app, Some(viewer("v@example.com")), "collections.list", json!({})).await;
    let list: Collections = serde_json::from_str(&text).unwrap();
    let names: Vec<&str> = list.collections.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Ghibli", "Rainy Sunday"]);
    let (_, text) = op_as(&app, Some(viewer("w@example.com")), "collections.list", json!({})).await;
    let list: Collections = serde_json::from_str(&text).unwrap();
    assert_eq!(list.collections.len(), 1, "another person's stays theirs");
    let (status, _) = op_as(
        &app,
        Some(viewer("w@example.com")),
        "collection.get",
        json!({"collection": mine.id}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let r = call(
        &app,
        Method::GET,
        &format!("/api/v1/collections/{}/items", mine.id),
        Some(viewer("w@example.com")),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // The agent reads it in order, with its notes; the app pages it.
    let (_, text) = op_as(
        &app,
        Some(owner()),
        "collection.get",
        json!({"collection": ghibli.id}),
    )
    .await;
    let got: CollectionItems = serde_json::from_str(&text).unwrap();
    let titles: Vec<&str> = got.items.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(titles, ["My Neighbor Totoro", "Spirited Away"]);
    assert_eq!(
        got.notes.get(&totoro.to_string()).map(String::as_str),
        Some("Where it began")
    );
    let r = call(
        &app,
        Method::GET,
        &format!("/api/v1/collections/{}/items?sort=year", ghibli.id),
        Some(viewer("w@example.com")),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let page: Page<ItemSummary> = r.json();
    let titles: Vec<&str> = page.items.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(titles, ["My Neighbor Totoro", "Spirited Away"]);

    // Removing everyone's takes the Owner or an Admin.
    let (status, _) = op_as(
        &app,
        Some(viewer("v@example.com")),
        "collection.remove",
        json!({"collection": ghibli.id}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, text) = op_as(
        &app,
        Some(owner()),
        "collection.remove",
        json!({"collection": ghibli.id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains("true"));
}
