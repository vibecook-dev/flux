//! The cross-site guard and the per-caller limits, through the router:
//! - review SEC-1: requests naming a foreign Host (DNS rebinding) get 421
//!   before any handler, so the localhost Owner never reaches them;
//! - review SEC-7: unsafe API calls need `X-Flux-Request: 1` and a foreign
//!   `Origin` is refused, so a body-less cross-site POST can't start a scan;
//! - roles are still enforced behind the guard;
//! - error bodies are plain text;
//! - event streams are capped per caller and in total (review SEC-3);
//! - `GET /libraries` reads the live scan status before the counts.

mod common;

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use common::*;
use flux_catalog::{Catalog, FileUpsert, ProbeUpdate, ScanBatch};
use flux_core::api::{Event, Id, Library, Role, ScanState, ScanStatus, ServerInfo};
use flux_core::LibraryKind;
use flux_net::{ConnInfo, NetPath};
use flux_server::scan::{Priority, ScanMode, ScanService};
use flux_server::{api::router, build_state, Config};
use serde_json::json;
use tokio::sync::broadcast;
use tower::ServiceExt;

/// A scanner that accepts every scan (so a request that passes the guard
/// gets 200, as in the reviewer's runs).
struct AcceptingScanner {
    tx: broadcast::Sender<Event>,
}

impl ScanService for AcceptingScanner {
    fn subscribe(&self) -> Option<broadcast::Receiver<Event>> {
        Some(self.tx.subscribe())
    }
    fn status(&self, _library_id: Id) -> Option<ScanStatus> {
        None
    }
    fn scan_library(&self, _library_id: Id, _mode: ScanMode, _priority: Priority) -> Result<(), String> {
        Ok(())
    }
    fn watch(&self, _library_id: Id) {}
    fn shutdown(&self, _timeout: Duration) {}
}

async fn app_with(cfg: Config) -> (Shared, Router) {
    let scanner: Arc<dyn ScanService> = Arc::new(AcceptingScanner {
        tx: broadcast::channel(16).0,
    });
    let (st, _) = build_state(cfg, Instant::now(), Some(scanner)).await.unwrap();
    let app = router(st.clone());
    (st, app)
}

fn library_json(name: &str, root: &str) -> serde_json::Value {
    json!({"name": name, "kind": "movies", "roots": [root]})
}

fn assert_plain_text(r: &Resp) {
    let ct = r.header("content-type").unwrap_or_default();
    assert!(
        ct.starts_with("text/plain"),
        "{} has content-type {ct:?}",
        r.status
    );
    assert!(
        !r.body.is_empty() && serde_json::from_slice::<serde_json::Value>(&r.body).is_err(),
        "{}",
        r.text()
    );
    assert_eq!(r.header("x-content-type-options").as_deref(), Some("nosniff"));
}

/// The reviewer's repro: `curl http://127.0.0.1:18421/api/v1/server -H 'Host:
/// rebind.attacker.example:18421'` returned the Owner, and the same Host
/// created a library rooted anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebinding_hosts_get_421_before_any_handler() {
    let dir = scratch("guard-host");
    let (st, app) = app_with(Config::for_tests(dir.path())).await;
    let evil = ("host", "rebind.attacker.example:18421");

    let r = call(&app, Method::GET, "/api/v1/server", Some(owner()), &[evil], None).await;
    assert_eq!(r.status, StatusCode::MISDIRECTED_REQUEST);
    assert_plain_text(&r);
    assert!(!r.text().contains("owner"), "{}", r.text());

    let outside = dir.path().join("outside");
    let pwned = library_json("Pwned", outside.to_str().unwrap());
    let r = call(
        &app,
        Method::POST,
        "/api/v1/libraries",
        Some(owner()),
        &[evil],
        Some(pwned),
    )
    .await;
    assert_eq!(r.status, StatusCode::MISDIRECTED_REQUEST);
    let libs: Vec<Library> = get(&app, "/api/v1/libraries").await.json();
    assert!(libs.iter().all(|l| l.name != "Pwned"), "{libs:?}");

    // Every route is behind it: admin, media, events, art, the web UI.
    for (method, uri, body) in [
        (Method::POST, "/api/v1/libraries/1/scan", None),
        (
            Method::POST,
            "/api/v1/tailnet/connect",
            Some(json!({"mode": "embedded"})),
        ),
        (Method::GET, "/api/v1/tailnet", None),
        (Method::GET, "/api/v1/events", None),
        (Method::GET, "/media/1/file", None),
        (Method::GET, "/media/1-abc/master.m3u8", None),
        (Method::GET, "/api/v1/art/1", None),
        (Method::GET, "/", None),
        (Method::GET, "/library/3", None),
    ] {
        let r = call(&app, method.clone(), uri, Some(owner()), &[evil], body).await;
        assert_eq!(r.status, StatusCode::MISDIRECTED_REQUEST, "{method} {uri}");
    }
    // An absolute-form request target names its host too.
    let r = call_raw(
        &app,
        Method::GET,
        "http://evil.example/api/v1/server",
        Some(owner()),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::MISDIRECTED_REQUEST);

    // The names this machine answers to, with any port.
    let host = flux_server::config::host_name().to_ascii_lowercase();
    let mut allowed = vec![
        "localhost".to_string(),
        "localhost:8096".into(),
        "127.0.0.1:18421".into(),
        "[::1]:8096".into(),
        format!("{host}:8096"),
        format!("{host}.local:8096"),
    ];
    if let Some(ip) = flux_server::guard::interface_ips()
        .into_iter()
        .find(|ip| !ip.is_loopback())
    {
        allowed.push(SocketAddr::new(ip, 8096).to_string());
    }
    for h in &allowed {
        let r = call(
            &app,
            Method::GET,
            "/api/v1/server",
            Some(owner()),
            &[("host", h.as_str())],
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "Host: {h}: {}", r.text());
        let me: ServerInfo = r.json();
        assert_eq!(me.you.map(|y| y.role), Some(Role::Owner), "{h}");
    }
    // No Host at all (an HTTP/1.0 tool, never a browser) passes.
    let r = call_raw(&app, Method::GET, "/api/v1/server", Some(owner()), &[], None).await;
    assert_eq!(r.status, StatusCode::OK);

    // The tailnet's names while it runs.
    let ts = ("host", "flux.tail1234.ts.net:8096");
    assert_eq!(
        call(&app, Method::GET, "/api/v1/server", None, &[ts], None)
            .await
            .status,
        StatusCode::MISDIRECTED_REQUEST
    );
    st.hosts
        .set_tailnet(&["flux.tail1234.ts.net".into()], &["100.64.0.7".parse().unwrap()]);
    for h in ["flux.tail1234.ts.net:8096", "100.64.0.7:8096", "flux:8096"] {
        let r = call(&app, Method::GET, "/api/v1/server", None, &[("host", h)], None).await;
        assert_eq!(r.status, StatusCode::OK, "{h}");
    }
    st.hosts.set_tailnet(&[], &[]);
    assert_eq!(
        call(&app, Method::GET, "/api/v1/server", None, &[ts], None)
            .await
            .status,
        StatusCode::MISDIRECTED_REQUEST
    );

    // --allow-host adds names.
    let mut cfg = Config::for_tests(dir.path().join("data2"));
    cfg.allow_hosts = vec!["media.example.com".into()];
    let (_, app2) = app_with(cfg).await;
    let proxied = ("host", "media.example.com");
    assert_eq!(
        call(&app2, Method::GET, "/api/v1/server", None, &[proxied], None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, Method::GET, "/api/v1/server", None, &[proxied], None)
            .await
            .status,
        StatusCode::MISDIRECTED_REQUEST
    );
}

/// Review SEC-7: `curl -X POST http://127.0.0.1:18431/api/v1/libraries/1/scan
/// -H 'Origin: https://evil.example' -H 'Sec-Fetch-Site: cross-site'`
/// started an Owner-only scan (200): the Host is legitimate and the request
/// has no body, so neither the Host check nor the JSON content type stops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_site_writes_are_refused() {
    let dir = scratch("guard-csrf");
    let (_st, app) = app_with(Config::for_tests(dir.path())).await;
    let lib = dir.path().join("films");
    let created: Library = post(
        &app,
        "/api/v1/libraries",
        library_json("Films", lib.to_str().unwrap()),
    )
    .await
    .json();
    let scan = format!("/api/v1/libraries/{}/scan", created.id);
    let local = ("host", "127.0.0.1:8096");
    let marker = ("x-flux-request", "1");
    let evil_origin = ("origin", "https://evil.example");
    let cross_site = ("sec-fetch-site", "cross-site");

    // The reviewer's request, exactly: no marker, foreign Origin.
    let r = call_raw(
        &app,
        Method::POST,
        &scan,
        Some(owner()),
        &[local, evil_origin, cross_site],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text());
    assert_plain_text(&r);
    // A foreign Origin is refused even with the marker…
    let r = call_raw(
        &app,
        Method::POST,
        &scan,
        Some(owner()),
        &[local, marker, evil_origin],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = call_raw(
        &app,
        Method::POST,
        &scan,
        Some(owner()),
        &[local, marker, ("origin", "null")],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // …and the marker is required without an Origin too (forms, curl).
    let r = call_raw(&app, Method::POST, &scan, Some(owner()), &[local], None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // The web client's request passes, same-origin or from the Vite dev server.
    for origin in [None, Some("http://127.0.0.1:8096"), Some("http://localhost:5173")] {
        let mut headers = vec![local, marker];
        if let Some(o) = origin {
            headers.push(("origin", o));
        }
        let r = call_raw(&app, Method::POST, &scan, Some(owner()), &headers, None).await;
        assert_eq!(r.status, StatusCode::OK, "{origin:?}: {}", r.text());
    }

    // Every unsafe API route needs the marker.
    let report = json!({"position_ms": 1, "paused": false, "ttff_ms": null, "last_seek_ms": null, "stalls": null, "dropped_frames": null});
    for (uri, body) in [
        ("/api/v1/libraries", Some(library_json("X", "/x"))),
        ("/api/v1/play", Some(json!({"item_id": 1}))),
        ("/api/v1/plan", Some(json!({"item_id": 1}))),
        ("/api/v1/items/1/watched", Some(json!({"watched": true}))),
        ("/api/v1/sessions/1.x.y/progress", Some(report)),
        ("/api/v1/tailnet/connect", Some(json!({"mode": "system"}))),
    ] {
        let r = call_raw(&app, Method::POST, uri, Some(owner()), &[local], body).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "POST {uri}");
    }
    for m in [Method::PUT, Method::PATCH, Method::DELETE] {
        let r = call_raw(
            &app,
            m.clone(),
            "/api/v1/libraries",
            Some(owner()),
            &[local],
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{m}");
    }
    // GETs need nothing.
    let r = call_raw(
        &app,
        Method::GET,
        "/api/v1/libraries",
        Some(owner()),
        &[local],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);

    // Roles still apply behind the guard: anonymous LAN callers can't scan.
    let r = call(&app, Method::POST, &scan, None, &[], None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.text().contains("owner"), "{}", r.text());
    let viewer = flux_core::api::Identity {
        role: Role::Viewer,
        ..owner()
    };
    let r = call(&app, Method::POST, &scan, Some(viewer), &[], None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_bodies_are_plain_text() {
    let dir = scratch("guard-errors");
    let (_st, app) = app_with(Config::for_tests(dir.path())).await;
    let bad_json = call(
        &app,
        Method::POST,
        "/api/v1/play",
        Some(owner()),
        &[("content-type", "application/json")],
        None,
    )
    .await;
    for r in [
        get(&app, "/api/v1/nope").await,
        get(&app, "/api/v1/items/99999999").await,
        get(&app, "/media/12345/file").await,
        bad_json,
        call(
            &app,
            Method::POST,
            "/api/v1/libraries",
            None,
            &[],
            Some(library_json("X", "/x")),
        )
        .await,
        call(&app, Method::GET, "/", None, &[("host", "evil.example")], None).await,
        call_raw(&app, Method::POST, "/api/v1/play", None, &[], None).await,
    ] {
        assert!(r.status.is_client_error(), "{}", r.status);
        assert_plain_text(&r);
    }
}

/// `GET /api/v1/events` as flux-net delivers it from `ip`. The response
/// (and with it the stream's slot) lives until it is dropped.
async fn events_from(app: &Router, ip: &str) -> axum::response::Response {
    let mut req = Request::builder()
        .uri("/api/v1/events")
        .header("host", "127.0.0.1:8096")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(None::<flux_core::api::Identity>);
    req.extensions_mut().insert(ConnInfo {
        path: NetPath::Lan,
        remote_addr: Some(SocketAddr::new(ip.parse::<IpAddr>().unwrap(), 50_000)),
        tailnet_name: None,
        grants: Vec::new(),
    });
    app.clone().oneshot(req).await.unwrap()
}

/// Review SEC-3 (server side): SSE streams never end on their own, so one
/// caller could hold any number of them. Now 8 per caller (a ninth ends the
/// caller's oldest, most likely a closed tab not yet noticed) and 32 in
/// total (past that, 503).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_streams_are_capped_per_caller_and_in_total() {
    use flux_server::events::{MAX_STREAMS, MAX_STREAMS_PER_IP};
    use http_body_util::BodyExt;
    let dir = scratch("guard-sse");
    let (st, app) = app_with(Config::for_tests(dir.path())).await;
    let mut open = Vec::new();
    for _ in 0..MAX_STREAMS_PER_IP {
        let r = events_from(&app, "192.168.1.10").await;
        assert_eq!(r.status(), StatusCode::OK);
        open.push(r);
    }
    // A ninth from the same caller replaces its oldest: that one ends.
    let newest = events_from(&app, "192.168.1.10").await;
    assert_eq!(newest.status(), StatusCode::OK);
    assert_eq!(st.events.open_streams(), MAX_STREAMS_PER_IP);
    let oldest = open.remove(0);
    let ended = tokio::time::timeout(Duration::from_secs(5), oldest.into_body().collect())
        .await
        .expect("the replaced stream ends")
        .unwrap()
        .to_bytes();
    assert!(String::from_utf8_lossy(&ended).contains("flux events"));
    open.push(newest);
    // A closed stream frees its slot.
    open.pop();
    assert_eq!(st.events.open_streams(), MAX_STREAMS_PER_IP - 1);
    let r = events_from(&app, "192.168.1.10").await;
    assert_eq!(r.status(), StatusCode::OK);
    open.push(r);
    // The total: other callers fill it, then everyone new gets 503.
    let mut n = 0u8;
    while open.len() < MAX_STREAMS {
        let r = events_from(&app, &format!("192.168.2.{}", 1 + n / 4)).await;
        assert_eq!(r.status(), StatusCode::OK);
        open.push(r);
        n += 1;
    }
    assert_eq!(st.events.open_streams(), MAX_STREAMS);
    let refused = events_from(&app, "192.168.3.1").await;
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(refused.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    // A caller at its own cap still swaps its oldest (the total doesn't grow).
    let r = events_from(&app, "192.168.1.10").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(st.events.open_streams(), MAX_STREAMS);
    open.push(r);
    drop(open);
    assert_eq!(st.events.open_streams(), 0);
}

/// A scanner whose scan finishes (and commits one more item) right when the
/// server asks for its status: the race the head-to-head benchmark hit.
struct FinishingScanner {
    /// The server's catalog, once it is open.
    catalog: OnceLock<Arc<Catalog>>,
    root_id: Id,
    done: AtomicBool,
}

impl ScanService for FinishingScanner {
    fn subscribe(&self) -> Option<broadcast::Receiver<Event>> {
        None
    }
    fn status(&self, _library_id: Id) -> Option<ScanStatus> {
        if !self.done.swap(true, Ordering::SeqCst) {
            let path = media("features/h264_truehd71.mkv");
            let rel = "h264_truehd71.mkv";
            let meta = std::fs::metadata(&path).unwrap();
            let upsert = FileUpsert {
                facts: facts(rel, &meta),
                probe: ProbeUpdate::Probed(Box::new(flux_probe::probe_file(&path).unwrap())),
                guess: Some(flux_names::parse(
                    std::path::Path::new(rel),
                    &[],
                    LibraryKind::Movies,
                )),
            };
            self.catalog
                .get()
                .expect("catalog")
                .writer()
                .apply(ScanBatch {
                    root_id: self.root_id,
                    upserts: vec![upsert],
                    ..Default::default()
                })
                .wait()
                .unwrap();
        }
        let mut s = flux_catalog::read::idle_status();
        s.state = ScanState::Idle;
        s.started_at = Some(1);
        s.finished_at = Some(flux_catalog::now_ms());
        Some(s)
    }
    fn scan_library(&self, _library_id: Id, _mode: ScanMode, _priority: Priority) -> Result<(), String> {
        Ok(())
    }
    fn watch(&self, _library_id: Id) {}
    fn shutdown(&self, _timeout: Duration) {}
}

/// A `GET /libraries` that reports a finished scan carries that scan's
/// counts (the benchmark saw "finished" with 298 of 300 shows).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_read_the_live_status_before_the_counts() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("guard-libcount");
    let cfg = Config::for_tests(dir.path());
    let (lib_id, root_id) = tokio::task::block_in_place(|| {
        let cat = Catalog::open(cfg.catalog_path(), flux_catalog::CatalogConfig::default()).unwrap();
        let lib = seed(&cat, "Features", LibraryKind::Movies, &media("features"), |n| {
            n == "h264_aac_faststart.mp4"
        });
        let root = cat
            .read_blocking(flux_catalog::scan_support::list_roots)
            .unwrap()
            .into_iter()
            .find(|r| r.library_id == lib)
            .unwrap()
            .id;
        cat.close().unwrap();
        (lib, root)
    });
    let scanner = Arc::new(FinishingScanner {
        catalog: OnceLock::new(),
        root_id,
        done: AtomicBool::new(false),
    });
    let (st, _) = build_state(cfg, Instant::now(), Some(scanner.clone()))
        .await
        .unwrap();
    let _ = scanner.catalog.set(st.catalog.clone());
    let app = router(st.clone());
    let libs: Vec<Library> = get(&app, "/api/v1/libraries").await.json();
    let lib = libs.iter().find(|l| l.id == lib_id).unwrap();
    assert!(lib.scan.finished_at.is_some(), "{:?}", lib.scan);
    assert_eq!(
        lib.item_count, 2,
        "a response that says finished must count what the scan committed"
    );
}
