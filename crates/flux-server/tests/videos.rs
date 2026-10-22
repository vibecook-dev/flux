//! A videos library through the API: created with kind `videos`, one item
//! per file, browsed with `GET /libraries/{id}/folders`.

mod common;

use std::time::Instant;

use axum::http::StatusCode;
use common::*;
use flux_core::api::{FolderPage, Library};
use flux_core::LibraryKind;
use flux_server::{api::router, build_state, Config};
use serde_json::json;

/// Percent-encodes a query value (folder tokens hold `/` and spaces).
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_videos_library_is_browsed_by_folder() {
    let dir = scratch("videos-folders");
    let root = dir.path().join("Home");
    for rel in [
        "Trip/IMG_0001.mp4",
        "Trip/IMG_0002.mp4",
        "Trip/Day 2/clip.mp4",
        // A trailer by its name in a movie library; here a video of its own.
        "Trip/Trip-trailer.mp4",
        "loose.mp4",
    ] {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, [0x1a_u8; 4096]).unwrap();
    }
    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    let lib = tokio::task::block_in_place(|| seed(&cat, "Home", LibraryKind::Videos, &root, |_| true));
    let app = router(st);

    let libs: Vec<Library> = get(&app, "/api/v1/libraries").await.json();
    let home = libs.iter().find(|l| l.id == lib).unwrap();
    assert_eq!((home.kind, home.item_count), (LibraryKind::Videos, 5));

    let r = get(&app, &format!("/api/v1/libraries/{lib}/folders")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let top: FolderPage = r.json();
    assert_eq!(top.at.name, "Home");
    assert!(top.parents.is_empty());
    let folders: Vec<(&str, u64)> = top
        .folders
        .iter()
        .map(|f| (f.name.as_str(), f.item_count))
        .collect();
    assert_eq!(folders, [("Trip", 4)]);
    let titles: Vec<&str> = top.items.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(titles, ["loose"]);
    // The wire shape the web client reads.
    let v: serde_json::Value = r.json();
    assert_eq!(v["folders"][0]["name"], "Trip");
    assert!(v["next"].is_null());

    let trip = &top.folders[0].folder;
    let r = get(
        &app,
        &format!(
            "/api/v1/libraries/{lib}/folders?folder={}&sort=-title&limit=2",
            enc(trip)
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let page: FolderPage = r.json();
    let titles: Vec<&str> = page.items.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(titles, ["Trip-trailer", "IMG 0002"]);
    assert_eq!((page.total, page.folders.len()), (3, 1));
    let next = page.next.clone().expect("a second page");
    let r = get(
        &app,
        &format!(
            "/api/v1/libraries/{lib}/folders?folder={}&sort=-title&limit=2&cursor={}",
            enc(trip),
            enc(&next)
        ),
    )
    .await;
    let page2: FolderPage = r.json();
    assert_eq!(
        page2.items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
        ["IMG 0001"]
    );
    assert_eq!(page2.next, None);
    assert_eq!(
        page2.parents.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["Home"]
    );

    // Unknown folders are 404, malformed ones 400, unknown libraries 404.
    let r = get(
        &app,
        &format!(
            "/api/v1/libraries/{lib}/folders?folder={}",
            enc(&format!("{trip}/Nope"))
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.text());
    let r = get(&app, &format!("/api/v1/libraries/{lib}/folders?folder=Trip")).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text());
    let r = get(&app, "/api/v1/libraries/999/folders").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.text());
    let r = get(&app, &format!("/api/v1/libraries/{lib}/folders?sort=newest")).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text());

    // The API creates videos libraries too.
    let other = dir.path().join("Clips");
    std::fs::create_dir_all(&other).unwrap();
    let r = post(
        &app,
        "/api/v1/libraries",
        json!({"name": "Clips", "kind": "videos", "roots": [other]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.json::<Library>().kind, LibraryKind::Videos);

    // Other kinds have no folder view (their episodes and extras are no
    // top-level items of a folder): 400, not a page that leaves them out.
    for kind in ["mixed", "shows", "movies"] {
        let other = dir.path().join(kind);
        std::fs::create_dir_all(&other).unwrap();
        let r = post(
            &app,
            "/api/v1/libraries",
            json!({"name": kind, "kind": kind, "roots": [other]}),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
        let id = r.json::<Library>().id;
        let r = get(&app, &format!("/api/v1/libraries/{id}/folders")).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{kind}: {}", r.text());
    }
}
