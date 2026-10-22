//! The real server on a real socket (flux-net, 127.0.0.1:0, tailnet off):
//! play the TrueHD MKV end to end, Range on the original file, SSE, and a
//! graceful shutdown with an event stream still open; the Host guard and
//! `--no-watch` through the real startup path.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use flux_catalog::{Catalog, CatalogConfig};
use flux_core::api::{Event, Id, PlayResponse, ScanState, ScanStatus, ServerInfo};
use flux_core::LibraryKind;
use flux_server::scan::{Priority, ScanMode, ScanService};
use flux_server::Config;
use tokio::sync::broadcast;

/// A scanner that reports a scan as soon as one is requested.
struct EchoScanner {
    tx: broadcast::Sender<Event>,
}

impl ScanService for EchoScanner {
    fn subscribe(&self) -> Option<broadcast::Receiver<Event>> {
        Some(self.tx.subscribe())
    }
    fn status(&self, _library_id: Id) -> Option<ScanStatus> {
        None
    }
    fn scan_library(&self, library_id: Id, _mode: ScanMode, _priority: Priority) -> Result<(), String> {
        let mut status = flux_catalog::read::idle_status();
        status.state = ScanState::Scanning;
        let _ = self.tx.send(Event::ScanProgress { library_id, status });
        Ok(())
    }
    fn watch(&self, _library_id: Id) {}
    fn shutdown(&self, _timeout: Duration) {}
}

/// A client that sends what flux's web client sends on every API call.
fn client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-flux-request", reqwest::header::HeaderValue::from_static("1"));
    reqwest::Client::builder()
        .no_proxy()
        .default_headers(headers)
        .build()
        .unwrap()
}

async fn fetch(c: &reqwest::Client, url: &str) -> (u16, Vec<u8>, Option<String>) {
    let r = c.get(url).send().await.expect("request");
    let status = r.status().as_u16();
    let len = r
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = r.bytes().await.expect("body").to_vec();
    (status, body, len)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_socket_play_range_sse_and_shutdown() {
    if !have_media() {
        eprintln!("skipping: corpus not mounted");
        return;
    }
    let dir = scratch("socket");
    let cfg = Config::for_tests(dir.path());
    // Seed, then close the catalog before the server opens it.
    let (lib_id, truehd, mp4) = tokio::task::block_in_place(|| {
        let cat = Catalog::open(cfg.catalog_path(), CatalogConfig::default()).unwrap();
        let lib = seed(&cat, "Features", LibraryKind::Movies, &media("features"), |n| {
            n == "h264_truehd71.mkv" || n == "h264_aac_faststart.mp4"
        });
        let t = item_of(&cat, "h264_truehd71.mkv");
        let m = item_of(&cat, "h264_aac_faststart.mp4");
        cat.close().unwrap();
        (lib, t, m)
    });

    let scanner: Arc<dyn ScanService> = Arc::new(EchoScanner {
        tx: broadcast::channel(16).0,
    });
    let t0 = Instant::now();
    let server = flux_server::start_with(cfg, t0, Some(scanner))
        .await
        .expect("server starts");
    assert!(server.ready_ms < 2000.0, "ready in {} ms", server.ready_ms);
    let base = format!("http://{}", server.local_addr);
    let c = client();

    let info: ServerInfo = c
        .get(format!("{base}/api/v1/server"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info.path, "localhost");
    assert_eq!(info.you.map(|y| y.role), Some(flux_core::api::Role::Owner));

    // SSE: connect, wait for the stream to open, trigger a scan, see the event.
    let mut sse = c.get(format!("{base}/api/v1/events")).send().await.unwrap();
    assert_eq!(
        sse.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let mut seen = String::new();
    while !seen.contains("flux events") {
        let chunk = tokio::time::timeout(Duration::from_secs(5), sse.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    let r = c
        .post(format!("{base}/api/v1/libraries/{lib_id}/scan"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let got_event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let chunk = sse.chunk().await.unwrap().unwrap();
            seen.push_str(&String::from_utf8_lossy(&chunk));
            if seen.contains("\"type\":\"scan_progress\"") {
                break;
            }
        }
    })
    .await;
    assert!(got_event.is_ok(), "no scan_progress on the event stream: {seen}");
    assert!(seen.contains(&format!("\"library_id\":{lib_id}")));

    // POST /play for the TrueHD MKV → master → variant → init → seg 0, plus audio.
    let play: PlayResponse = c
        .post(format!("{base}/api/v1/play"))
        .json(&serde_json::json!({
            "item_id": truehd.0, "asset_id": null, "caps": chrome_caps(),
            "audio_track": null, "subtitle_track": null, "start_ms": null
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let t_play = Instant::now();
    let (s, master, _) = fetch(&c, &format!("{base}{}", play.url)).await;
    assert_eq!(s, 200);
    let master = String::from_utf8(master).unwrap();
    let stream = format!("{base}{}", play.url.trim_end_matches("master.m3u8"));
    let variant = master
        .lines()
        .find(|l| !l.starts_with('#') && !l.trim().is_empty())
        .unwrap()
        .to_string();
    let (s, vtext, _) = fetch(&c, &format!("{stream}{variant}")).await;
    assert_eq!(s, 200);
    let seg0 = String::from_utf8(vtext)
        .unwrap()
        .lines()
        .find(|l| l.ends_with(".m4s"))
        .unwrap()
        .to_string();
    let vdir = format!("{stream}{}", variant.trim_end_matches("index.m3u8"));
    let (s, init, len) = fetch(&c, &format!("{vdir}init.mp4")).await;
    assert_eq!((s, len), (200, Some(init.len().to_string())));
    let (s, seg, len) = fetch(&c, &format!("{vdir}{seg0}")).await;
    assert_eq!((s, len), (200, Some(seg.len().to_string())));
    let ttfs = t_play.elapsed();
    let audio = master
        .lines()
        .find(|l| l.starts_with("#EXT-X-MEDIA:TYPE=AUDIO") && l.contains("DEFAULT=YES"))
        .and_then(|l| l.split("URI=\"").nth(1)?.split('"').next())
        .expect("audio rendition")
        .to_string();
    let adir = format!("{stream}{}", audio.trim_end_matches("index.m3u8"));
    let (s, _, _) = fetch(&c, &format!("{stream}{audio}")).await;
    assert_eq!(s, 200);
    let (s, _, _) = fetch(&c, &format!("{adir}init.mp4")).await;
    assert_eq!(s, 200);
    let (s, aseg, len) = fetch(&c, &format!("{adir}0.m4s")).await;
    assert_eq!((s, len), (200, Some(aseg.len().to_string())));
    eprintln!("TTFS (video) over the socket: {ttfs:?}");

    // Range on the original file.
    let r = c
        .get(format!("{base}/media/{}/file", mp4.1))
        .header("range", "bytes=100-299")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 206);
    let body = r.bytes().await.unwrap();
    let on_disk = std::fs::read(media("features/h264_aac_faststart.mp4")).unwrap();
    assert_eq!(&body[..], &on_disk[100..300]);

    // Graceful shutdown completes promptly with the SSE stream still open.
    let t = Instant::now();
    server.shutdown(Duration::from_secs(5)).await;
    assert!(
        t.elapsed() < Duration::from_secs(4),
        "shutdown took {:?}",
        t.elapsed()
    );
    let tail = tokio::time::timeout(Duration::from_secs(2), sse.chunk()).await;
    assert!(
        matches!(tail, Ok(Ok(None)) | Ok(Err(_))),
        "event stream should end: {tail:?}"
    );
}

/// The real flux-scan orchestrator behind the server: a startup scan fills
/// the catalog and reports progress over SSE.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_scan_with_flux_scan() {
    if !have_media() {
        return;
    }
    let dir = scratch("socket-scan");
    let mut cfg = Config::for_tests(dir.path());
    cfg.scan = true;
    cfg.libraries = vec![
        format!("Movies:movies:{}", media("library-tiny/Movies").display())
            .parse()
            .unwrap(),
    ];
    let server = flux_server::start(cfg).await.unwrap();
    let base = format!("http://{}", server.local_addr);
    let c = client();
    let mut sse = c.get(format!("{base}/api/v1/events")).send().await.unwrap();
    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !seen.contains("scan_progress") && Instant::now() < deadline {
        if let Ok(Ok(Some(chunk))) = tokio::time::timeout(Duration::from_secs(5), sse.chunk()).await {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    assert!(seen.contains("scan_progress"));
    // The startup catch-up scan finishes and leaves the library browsable
    // and watched.
    let deadline = Instant::now() + Duration::from_secs(60);
    let lib = loop {
        let libs: Vec<flux_core::api::Library> = c
            .get(format!("{base}/api/v1/libraries"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let lib = libs.into_iter().next().expect("the configured library");
        if lib.scan.finished_at.is_some() && lib.scan.state != ScanState::Scanning {
            break lib;
        }
        assert!(Instant::now() < deadline, "scan didn't finish: {:?}", lib.scan);
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(lib.item_count > 0, "{lib:?}");
    assert_eq!(lib.scan.last_error, None);
    assert!(
        lib.scan.files_seen > 0 && lib.scan.items_added > 0,
        "{:?}",
        lib.scan
    );
    let page: flux_core::api::Page<flux_core::api::ItemSummary> = c
        .get(format!("{base}/api/v1/libraries/{}/items?limit=500", lib.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len() as u64, lib.item_count);
    server.shutdown(Duration::from_secs(5)).await;
}

/// Sends raw HTTP/1.1 on a fresh connection and returns (status, body).
async fn raw_request(addr: std::net::SocketAddr, request: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw))
        .await
        .expect("response")
        .unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, body.to_string())
}

/// Review SEC-1 on the real listener, the reviewer's curl repro: a forged
/// Host on 127.0.0.1 got the Owner and could create a library anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_socket_refuses_foreign_hosts() {
    let dir = scratch("socket-host");
    let server = flux_server::start(Config::for_tests(dir.path())).await.unwrap();
    let addr = server.local_addr;
    let port = addr.port();

    let (status, body) = raw_request(
        addr,
        &format!("GET /api/v1/server HTTP/1.1\r\nHost: rebind.attacker.example:{port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 421, "{body}");
    assert!(!body.contains("owner"), "{body}");
    let lib = format!(
        r#"{{"name":"Pwned","kind":"movies","roots":["{}"]}}"#,
        dir.path().display()
    );
    let (status, _) = raw_request(
        addr,
        &format!(
            "POST /api/v1/libraries HTTP/1.1\r\nHost: evil.example:{port}\r\nX-Flux-Request: 1\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{lib}",
            lib.len()
        ),
    )
    .await;
    assert_eq!(status, 421);

    // The real names work, and localhost is the owner there.
    for host in [format!("127.0.0.1:{port}"), format!("localhost:{port}")] {
        let (status, body) = raw_request(
            addr,
            &format!("GET /api/v1/server HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status, 200, "{host}: {body}");
        let info: ServerInfo = serde_json::from_str(&body).unwrap();
        assert_eq!(info.you.map(|y| y.role), Some(flux_core::api::Role::Owner));
    }
    let libs: Vec<flux_core::api::Library> = client()
        .get(format!("http://{addr}/api/v1/libraries"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(libs.is_empty(), "{libs:?}");
    server.shutdown(Duration::from_secs(5)).await;
}

/// Records what the server asks of the scanner.
#[derive(Default)]
struct RecordingScanner {
    calls: Mutex<Vec<String>>,
}

impl ScanService for RecordingScanner {
    fn subscribe(&self) -> Option<broadcast::Receiver<Event>> {
        None
    }
    fn status(&self, _library_id: Id) -> Option<ScanStatus> {
        None
    }
    fn scan_library(&self, library_id: Id, mode: ScanMode, priority: Priority) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("scan {library_id} {mode:?} {priority:?}"));
        Ok(())
    }
    fn watch(&self, library_id: Id) {
        self.calls.lock().unwrap().push(format!("watch {library_id}"));
    }
    fn watch_without_catch_up(&self, library_id: Id) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("watch {library_id} without catch-up"));
    }
    fn shutdown(&self, _timeout: Duration) {}
}

/// `--no-watch`: the startup scan still runs (a Quick scan at Scheduled
/// priority, which watching used to start), "Scan now" scans, and nothing
/// is watched. With watching on, startup watches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_watch_scans_at_startup_without_watching() {
    for watch in [false, true] {
        let dir = scratch("socket-watch");
        let films = dir.path().join("films");
        std::fs::create_dir_all(&films).unwrap();
        let mut cfg = Config::for_tests(dir.path().join("data"));
        cfg.scan = true;
        cfg.watch = watch;
        cfg.libraries = vec![format!("Films:movies:{}", films.display()).parse().unwrap()];
        let scanner = Arc::new(RecordingScanner::default());
        let server = flux_server::start_with(cfg, Instant::now(), Some(scanner.clone()))
            .await
            .unwrap();
        let startup = eventually(10, || {
            let calls = scanner.calls.lock().unwrap().clone();
            (!calls.is_empty()).then_some(calls)
        })
        .await
        .expect("the startup scan");
        let lib: Vec<flux_core::api::Library> = client()
            .get(format!("http://{}/api/v1/libraries", server.local_addr))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = lib[0].id;
        if watch {
            assert_eq!(startup, [format!("watch {id}")]);
        } else {
            assert_eq!(startup, [format!("scan {id} Quick Scheduled")]);
            let r = client()
                .post(format!("http://{}/api/v1/libraries/{id}/scan", server.local_addr))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 200);
            let calls = scanner.calls.lock().unwrap().clone();
            assert!(calls.iter().all(|c| !c.starts_with("watch")), "{calls:?}");
            assert_eq!(calls.len(), 2, "{calls:?}");
        }
        server.shutdown(Duration::from_secs(5)).await;
    }
}

/// After an upgrade (a file parsed by an older flux-names), the startup
/// scan is a Deep one, which re-parses it: a Quick scan never re-reads an
/// unchanged file. With watching on, the watcher starts first, without its
/// catch-up: joining the Deep scan once it had started, the catch-up ran a
/// second pass that counted every file twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_scan_is_deep_after_a_parser_upgrade() {
    for watch in [false, true] {
        let dir = scratch("socket-upgrade");
        let films = dir.path().join("films");
        std::fs::create_dir_all(&films).unwrap();
        let mut cfg = Config::for_tests(dir.path().join("data"));
        cfg.scan = true;
        cfg.watch = watch;
        std::fs::create_dir_all(&cfg.data_dir).unwrap();
        // A catalog an older flux wrote: one file at the previous parser version.
        let id = tokio::task::block_in_place(|| {
            let cat = Catalog::open(cfg.catalog_path(), CatalogConfig::default()).unwrap();
            let lib = cat
                .writer()
                .create_library(flux_core::api::CreateLibrary {
                    name: "Films".into(),
                    kind: LibraryKind::Movies,
                    roots: vec![films.to_string_lossy().into_owned()],
                })
                .wait()
                .unwrap();
            let lib_id = lib.id;
            cat.writer()
                .exec(move |c| {
                    let root: i64 =
                        c.query_row("SELECT id FROM root WHERE library_id = ?1", [lib_id], |r| {
                            r.get(0)
                        })?;
                    c.execute(
                        "INSERT INTO file(root_id, rel_path, size, mtime_ns, state, probe_version,
                                          parse_version, first_seen_at, updated_at)
                         VALUES(?1, 'Some Film (2019).mkv', 1, 1, 'present', ?2, ?3, 0, 0)",
                        (root, flux_core::PROBE_VERSION, flux_core::PARSER_VERSION - 1),
                    )?;
                    Ok(())
                })
                .wait()
                .unwrap();
            cat.close().unwrap();
            lib_id
        });
        let scanner = Arc::new(RecordingScanner::default());
        let server = flux_server::start_with(cfg, Instant::now(), Some(scanner.clone()))
            .await
            .unwrap();
        let want = if watch { 2 } else { 1 };
        let startup = eventually(10, || {
            let calls = scanner.calls.lock().unwrap().clone();
            (calls.len() >= want).then_some(calls)
        })
        .await
        .expect("the startup scan");
        let mut expect = Vec::new();
        if watch {
            expect.push(format!("watch {id} without catch-up"));
        }
        expect.push(format!("scan {id} Deep Scheduled"));
        assert_eq!(startup, expect);
        server.shutdown(Duration::from_secs(5)).await;
    }
}
