//! Router tests (`tower::ServiceExt::oneshot`) over catalogs seeded from the
//! real corpus: browse, libraries, auth, tailnet, errors and the web UI.

mod common;

use std::time::Instant;

use axum::http::{Method, StatusCode};
use common::*;
use flux_core::api::{
    Home, ItemDetail, ItemKind, ItemSummary, Library, Page, ServerInfo, TailnetInfo, TailnetMode,
    TailnetState,
};
use flux_core::LibraryKind;
use flux_server::{api::router, build_state, Config};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_routes_over_library_tiny() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("browse");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let cat = st.catalog.clone();
    let (movies, tv) = tokio::task::block_in_place(|| {
        (
            seed(
                &cat,
                "Movies",
                LibraryKind::Movies,
                &media("library-tiny/Movies"),
                |_| true,
            ),
            seed(&cat, "TV", LibraryKind::Shows, &media("library-tiny/TV"), |_| {
                true
            }),
        )
    });
    let app = router(st.clone());

    // Who am I, and how did I connect?
    let me: ServerInfo = get(&app, "/api/v1/server").await.json();
    assert_eq!(me.path, "localhost");
    assert_eq!(me.you.map(|y| y.login_name).as_deref(), Some("owner@example.com"));
    let anon: ServerInfo = call(&app, Method::GET, "/api/v1/server", None, &[], None)
        .await
        .json();
    assert_eq!((anon.path.as_str(), anon.you), ("lan", None));

    let libs: Vec<Library> = get(&app, "/api/v1/libraries").await.json();
    assert_eq!(libs.len(), 2);
    assert!(
        libs.iter().all(|l| l.item_count > 0 && l.roots[0].available),
        "{libs:?}"
    );

    // Keyset pages by title cover the library exactly once.
    let first: Page<ItemSummary> = get(&app, &format!("/api/v1/libraries/{movies}/items?limit=10"))
        .await
        .json();
    let total = first.total.expect("total on the first page") as usize;
    assert_eq!(first.items.len(), 10);
    assert!(total > 10, "{total}");
    let mut seen: Vec<i64> = first.items.iter().map(|i| i.id).collect();
    let mut next = first.next.clone();
    while let Some(cursor) = next {
        let p: Page<ItemSummary> = get(
            &app,
            &format!("/api/v1/libraries/{movies}/items?limit=10&cursor={cursor}"),
        )
        .await
        .json();
        seen.extend(p.items.iter().map(|i| i.id));
        next = p.next;
    }
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!((seen.len(), unique.len()), (total, total));

    let recent = get(
        &app,
        &format!("/api/v1/libraries/{movies}/items?sort=-added&filter=4k"),
    )
    .await;
    assert_eq!(recent.status, StatusCode::OK, "{}", recent.text());
    // Every sort pages through the library exactly once, in its order.
    for sort in ["title", "-title", "year", "-year", "added", "-added"] {
        let mut seen: Vec<ItemSummary> = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let cursor = next
                .as_deref()
                .map(|c| format!("&cursor={c}"))
                .unwrap_or_default();
            let r = get(
                &app,
                &format!("/api/v1/libraries/{movies}/items?limit=7&sort={sort}{cursor}"),
            )
            .await;
            assert_eq!(r.status, StatusCode::OK, "{sort}: {}", r.text());
            let p: Page<ItemSummary> = r.json();
            seen.extend(p.items);
            next = p.next;
            if next.is_none() {
                break;
            }
        }
        let mut ids: Vec<i64> = seen.iter().map(|i| i.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!((seen.len(), ids.len()), (total, total), "{sort}");
        let years: Vec<Option<u16>> = seen.iter().map(|i| i.year).collect();
        let known: Vec<u16> = years.iter().flatten().copied().collect();
        match sort {
            "year" => assert!(known.windows(2).all(|w| w[0] <= w[1]), "{sort}: {years:?}"),
            "-year" => assert!(known.windows(2).all(|w| w[0] >= w[1]), "{sort}: {years:?}"),
            _ => {}
        }
        if sort.ends_with("year") {
            // Unknown years sort last.
            let first_none = years.iter().position(Option::is_none).unwrap_or(years.len());
            assert!(
                years[first_none..].iter().all(Option::is_none),
                "{sort}: {years:?}"
            );
        }
    }
    // A cursor belongs to its sort.
    let by_year: Page<ItemSummary> = get(
        &app,
        &format!("/api/v1/libraries/{movies}/items?limit=3&sort=year"),
    )
    .await
    .json();
    let cursor = by_year.next.expect("more than one page");
    let r = get(
        &app,
        &format!("/api/v1/libraries/{movies}/items?sort=title&cursor={cursor}"),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text());
    assert_eq!(
        get(&app, &format!("/api/v1/libraries/{movies}/items?sort=bogus"))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&app, "/api/v1/libraries/999/items").await.status,
        StatusCode::NOT_FOUND
    );

    let home: Home = get(&app, "/api/v1/home").await.json();
    assert!(!home.recently_added.is_empty());
    assert_eq!(home.rows.len(), 2);

    // A movie's detail carries its files and their probes.
    let movie = first
        .items
        .iter()
        .find(|i| i.kind == ItemKind::Movie)
        .expect("a movie");
    let detail: ItemDetail = get(&app, &format!("/api/v1/items/{}", movie.id)).await.json();
    assert!(!detail.assets.is_empty() && detail.assets[0].probe.is_some());
    assert_eq!(
        get(&app, "/api/v1/items/99999999").await.status,
        StatusCode::NOT_FOUND
    );

    // Show → seasons → episodes.
    let shows: Page<ItemSummary> = get(&app, &format!("/api/v1/libraries/{tv}/items?limit=5"))
        .await
        .json();
    let show = shows
        .items
        .iter()
        .find(|i| i.kind == ItemKind::Show)
        .expect("a show");
    let seasons: Vec<ItemSummary> = get(&app, &format!("/api/v1/items/{}/children", show.id))
        .await
        .json();
    assert!(!seasons.is_empty() && seasons.iter().all(|s| s.kind == ItemKind::Season));
    let episodes: Vec<ItemSummary> = get(&app, &format!("/api/v1/items/{}/children", seasons[0].id))
        .await
        .json();
    assert!(!episodes.is_empty() && episodes.iter().all(|e| e.kind == ItemKind::Episode));
    assert_eq!(
        get(&app, "/api/v1/items/99999999/children").await.status,
        StatusCode::NOT_FOUND
    );

    // Search by the first word of a title.
    let word = movie.title.split_whitespace().next().unwrap().to_string();
    let hits: Vec<ItemSummary> = get(&app, &format!("/api/v1/search?q={}", urlencode(&word)))
        .await
        .json();
    assert!(hits.iter().any(|h| h.id == movie.id), "{word}: {hits:?}");
    let none: Vec<ItemSummary> = get(&app, "/api/v1/search?q=").await.json();
    assert!(none.is_empty());

    // Mark watched and back.
    let r = post(
        &app,
        &format!("/api/v1/items/{}/watched", movie.id),
        json!({"watched": true}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let d: ItemDetail = get(&app, &format!("/api/v1/items/{}", movie.id)).await.json();
    assert!(d.summary.progress.expect("progress").watched);
    let r = post(
        &app,
        &format!("/api/v1/items/{}/watched", movie.id),
        json!({"watched": false}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        post(&app, "/api/v1/items/99999999/watched", json!({"watched": true}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // Anonymous callers share one guest user; their state is separate.
    let anon_detail: ItemDetail = call(
        &app,
        Method::GET,
        &format!("/api/v1/items/{}", movie.id),
        None,
        &[],
        None,
    )
    .await
    .json();
    assert!(anon_detail.summary.progress.is_none_or(|p| !p.watched));
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_routes_auth_tailnet_and_errors() {
    let dir = scratch("admin");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st.clone());
    let lib = json!({"name": "Films", "kind": "movies", "roots": ["/nonexistent/films"]});

    // Only the owner/admin creates libraries; LAN callers are anonymous.
    let r = call(
        &app,
        Method::POST,
        "/api/v1/libraries",
        None,
        &[],
        Some(lib.clone()),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let created: Library = post(&app, "/api/v1/libraries", lib).await.json();
    assert_eq!(
        (created.name.as_str(), created.kind),
        ("Films", LibraryKind::Movies)
    );
    let r = post(
        &app,
        "/api/v1/libraries",
        json!({"name": "X", "kind": "movies", "roots": ["relative"]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = post(
        &app,
        "/api/v1/libraries",
        json!({"name": "X", "kind": "films", "roots": ["/x"]}),
    )
    .await;
    assert!(r.status.is_client_error());

    // Scanning is off in tests: 503 with the reason; unknown library 404.
    let r = post(
        &app,
        &format!("/api/v1/libraries/{}/scan", created.id),
        json!(null),
    )
    .await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.text().contains("turned off"), "{}", r.text());
    assert_eq!(
        post(&app, "/api/v1/libraries/999/scan", json!(null)).await.status,
        StatusCode::NOT_FOUND
    );

    // Tailnet: off; switching needs the owner; same mode is a no-op.
    let t: TailnetInfo = get(&app, "/api/v1/tailnet").await.json();
    assert_eq!((t.mode, t.state), (TailnetMode::Off, TailnetState::Off));
    let r = call(
        &app,
        Method::POST,
        "/api/v1/tailnet/connect",
        None,
        &[],
        Some(json!({"mode": "system"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let t: TailnetInfo = post(&app, "/api/v1/tailnet/connect", json!({"mode": "off"}))
        .await
        .json();
    assert_eq!(t.state, TailnetState::Off);
    // No listener supervisor behind a bare router: refused, nothing remembered.
    let r = post(&app, "/api/v1/tailnet/connect", json!({"mode": "system"})).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!dir.path().join("tailnet-mode").exists());

    // Errors.
    assert_eq!(get(&app, "/api/v1/nope").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&app, "/media/1-abc/master.m3u8").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(&app, "/media/12345/file").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&app, "/media/../../etc/passwd").await.status,
        StatusCode::NOT_FOUND
    );
    let r = call(
        &app,
        Method::POST,
        "/api/v1/play",
        Some(owner()),
        &[("content-type", "application/json")],
        None,
    )
    .await;
    assert!(r.status.is_client_error(), "{}", r.status);
    let r = post(
        &app,
        "/api/v1/play",
        json!({"item_id": 1, "asset_id": null, "caps": chrome_caps(), "audio_track": null, "subtitle_track": null, "start_ms": null}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.text());
    let r = post(
        &app,
        "/api/v1/sessions/garbage/progress",
        json!({"position_ms": 1, "paused": false, "ttff_ms": null, "last_seek_ms": null, "stalls": null, "dropped_frames": null}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // No web client configured: a short page that says how to build it.
    let r = get(&app, "/").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.text().contains("pnpm build"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_ui_spa_csp_and_caching() {
    let dir = scratch("webui");
    let web = dir.path().join("dist");
    std::fs::create_dir_all(web.join("assets")).unwrap();
    let theme = "\n      (function () { document.documentElement.dataset.theme = 'dark'; })();\n    ";
    std::fs::write(
        web.join("index.html"),
        format!(
            "<!doctype html><html><head><script>{theme}</script>\
             <script type=\"module\" crossorigin src=\"/assets/index-abc123.js\"></script></head>\
             <body><div id=\"root\"></div></body></html>"
        ),
    )
    .unwrap();
    let js = "console.log('flux');\n".repeat(400);
    std::fs::write(web.join("assets/index-abc123.js"), &js).unwrap();
    std::fs::write(
        web.join("favicon.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    )
    .unwrap();
    let mut cfg = Config::for_tests(dir.path().join("data"));
    cfg.web_dir = Some(web.clone());
    let (st, _) = build_state(cfg, Instant::now(), None).await.unwrap();
    let app = router(st.clone());

    let index = get(&app, "/").await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.header("content-type").unwrap().starts_with("text/html"));
    assert_eq!(index.header("cache-control").as_deref(), Some("no-cache"));
    let csp = index.header("content-security-policy").expect("CSP");
    use base64::Engine;
    use sha2::Digest;
    let hash = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(theme.as_bytes()));
    assert!(csp.contains(&format!("'sha256-{hash}'")), "{csp}");
    assert!(!csp.contains("unsafe-inline") && csp.contains("frame-ancestors 'none'"));

    // Client-side routes get the shell; hashed assets are immutable.
    let spa = get(&app, "/library/3").await;
    assert_eq!(
        (spa.status, spa.body.clone()),
        (StatusCode::OK, index.body.clone())
    );
    let a = get(&app, "/assets/index-abc123.js").await;
    assert_eq!(
        a.header("cache-control").as_deref(),
        Some("public, max-age=31536000, immutable")
    );
    assert!(a.header("content-type").unwrap().starts_with("text/javascript"));
    assert_eq!(a.body.len(), js.len());
    let gz = call(
        &app,
        Method::GET,
        "/assets/index-abc123.js",
        Some(owner()),
        &[("accept-encoding", "gzip, br")],
        None,
    )
    .await;
    assert_eq!(gz.header("content-encoding").as_deref(), Some("gzip"));
    assert!(gz.body.len() < js.len() / 10);
    let etag = a.header("etag").unwrap();
    let again = call(
        &app,
        Method::GET,
        "/assets/index-abc123.js",
        Some(owner()),
        &[("if-none-match", &etag)],
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert_eq!(
        get(&app, "/assets/missing-123.js").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app, "/favicon.svg").await.header("cache-control").as_deref(),
        Some("no-cache")
    );
    // The API still answers 404 under /api, never the shell.
    assert_eq!(get(&app, "/api/v1/whatever").await.status, StatusCode::NOT_FOUND);
}
