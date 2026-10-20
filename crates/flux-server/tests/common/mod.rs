//! Test helpers: scratch dirs, catalogs seeded the way flux-scan
//! will fill them (flux-names + flux-probe → `Writer::apply`, as
//! flux-scan/examples/pipeline_smoke.rs does), and request helpers.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::Router;
use bytes::Bytes;
use flux_catalog::{scan_support, Catalog, FileFacts, FileUpsert, ProbeUpdate, ScanBatch};
use flux_core::api::{CreateLibrary, Id, Identity, Role};
use flux_core::LibraryKind;
use flux_net::{ConnInfo, NetPath};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A corpus path, `$FLUX_TEST_MEDIA/<rel>` (one that never exists when
/// the variable is unset).
pub fn media(rel: &str) -> PathBuf {
    flux_test_media::media_dir(rel)
}

/// `true` when the corpus is there (tests skip otherwise, saying why).
pub fn have_media() -> bool {
    flux_test_media::media("features/h264_truehd71.mkv").is_some()
        && flux_test_media::media("library-tiny/Movies").is_some()
}

/// A fresh scratch directory in the temp dir (`TMPDIR`).
pub fn scratch(name: &str) -> tempfile::TempDir {
    let base = std::env::temp_dir().join("flux-server-tests");
    let _ = std::fs::create_dir_all(&base);
    tempfile::Builder::new()
        .prefix(&format!("{name}-"))
        .tempdir_in(base)
        .expect("scratch dir")
}

/// The localhost owner identity flux-net attaches on 127.0.0.1.
pub fn owner() -> Identity {
    Identity {
        login_name: "owner@example.com".into(),
        display_name: "Owner".into(),
        profile_pic_url: None,
        node_name: None,
        role: Role::Owner,
        tags: Vec::new(),
    }
}

/// Seeds a library from a folder: every video file under `root` (filtered by
/// `keep(file name)`), named by flux-names and probed by flux-probe.
/// Returns the library id.
pub fn seed(cat: &Catalog, name: &str, kind: LibraryKind, root: &Path, keep: impl Fn(&str) -> bool) -> Id {
    let lib = cat
        .writer()
        .create_library(CreateLibrary {
            name: name.into(),
            kind,
            roots: vec![root.to_string_lossy().into_owned()],
        })
        .wait()
        .expect("create library");
    let root_id = cat
        .read_blocking(move |c| {
            Ok(scan_support::list_roots(c)?
                .into_iter()
                .find(|r| r.library_id == lib.id)
                .map(|r| r.id))
        })
        .expect("roots")
        .expect("root");
    let mut files = Vec::new();
    walk(root, &mut files);
    let mut by_dir: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for f in &files {
        by_dir
            .entry(f.parent().unwrap_or(root).to_path_buf())
            .or_default()
            .push(f.file_name().unwrap_or_default().to_string_lossy().into_owned());
    }
    let mut upserts = Vec::new();
    for f in &files {
        let fname = f.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let rel = f
            .strip_prefix(root)
            .unwrap_or(f)
            .to_string_lossy()
            .replace('\\', "/");
        if !flux_names::is_video_file(&fname) || flux_names::is_ignored(Path::new(&rel)) || !keep(&fname) {
            continue;
        }
        let sibs: Vec<&str> = by_dir
            .get(f.parent().unwrap_or(root))
            .map(|v| v.iter().map(String::as_str).filter(|n| *n != fname).collect())
            .unwrap_or_default();
        let guess = flux_names::parse(Path::new(&rel), &sibs, kind);
        let probe = match flux_probe::probe_file(f) {
            Ok(p) => ProbeUpdate::Probed(Box::new(p)),
            Err(e) => ProbeUpdate::Failed(e.to_string()),
        };
        let meta = std::fs::metadata(f).expect("stat");
        upserts.push(FileUpsert {
            facts: facts(&rel, &meta),
            probe,
            guess: Some(guess),
        });
    }
    for chunk in upserts.chunks(500) {
        cat.writer()
            .apply(ScanBatch {
                root_id,
                upserts: chunk.to_vec(),
                ..Default::default()
            })
            .wait()
            .expect("apply");
    }
    lib.id
}

pub fn facts(rel: &str, meta: &std::fs::Metadata) -> FileFacts {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileFacts {
            rel_path: rel.to_string(),
            dev: Some(meta.dev()),
            ino: Some(meta.ino()),
            btime_ns: None,
            size: meta.len(),
            mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            fp128: None,
            oshash: None,
        }
    }
    #[cfg(not(unix))]
    {
        FileFacts {
            btime_ns: None,
            ..flux_catalog::synth::facts_from_metadata(rel, meta)
        }
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, out),
            Ok(t) if t.is_file() => out.push(p),
            // A file link is cataloged under its own path with its target's
            // stat, as flux-scan does for links that leave the roots.
            Ok(t) if t.is_symlink() && p.is_file() => out.push(p),
            _ => {}
        }
    }
}

/// `(item_id, asset_id)` of the file whose relative path ends with `name`.
pub fn item_of(cat: &Catalog, name: &str) -> (Id, Id) {
    let pat = format!("%{name}");
    cat.read_blocking(move |c| {
        Ok(c.query_row(
            "SELECT a.item_id, a.id FROM asset a JOIN file f ON f.id = a.file_id WHERE f.rel_path LIKE ?1",
            [pat],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    })
    .expect("asset for file")
}

/// Re-probes `path` and upserts it at `rel` under `root_id` (as a rescan of
/// one file would), so a test can take a file from `probe_failed` back to
/// `present`.
pub fn reseed_file(cat: &Catalog, root_id: Id, rel: &str, path: &Path) {
    let probe = match flux_probe::probe_file(path) {
        Ok(p) => ProbeUpdate::Probed(Box::new(p)),
        Err(e) => ProbeUpdate::Failed(e.to_string()),
    };
    let meta = std::fs::metadata(path).expect("stat");
    cat.writer()
        .apply(ScanBatch {
            root_id,
            upserts: vec![FileUpsert {
                facts: facts(rel, &meta),
                probe,
                guess: None,
            }],
            ..Default::default()
        })
        .wait()
        .expect("reseed");
}

/// The root id an asset's file is cataloged under.
pub fn root_of(cat: &Catalog, asset: Id) -> Id {
    cat.read_blocking(move |c| {
        Ok(c.query_row(
            "SELECT f.root_id FROM asset a JOIN file f ON f.id = a.file_id WHERE a.id = ?1",
            [asset],
            |r| r.get(0),
        )?)
    })
    .expect("root for asset")
}

/// A response, collected.
pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Resp {
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "bad JSON ({e}) for status {}: {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    }
}

/// Sends one request through the router as flux-net would (identity +
/// conn), the way the web client on this machine sends it: with
/// `Host: 127.0.0.1:8096` and `X-Flux-Request: 1` unless `headers` sets them.
pub async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    who: Option<Identity>,
    headers: &[(&str, &str)],
    body: Option<serde_json::Value>,
) -> Resp {
    let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    let mut all: Vec<(&str, &str)> = Vec::new();
    if !has("host") {
        all.push(("host", "127.0.0.1:8096"));
    }
    if !has("x-flux-request") {
        all.push(("x-flux-request", "1"));
    }
    all.extend_from_slice(headers);
    call_raw(router, method, uri, who, &all, body).await
}

/// [`call`] with exactly `headers` (no Host or X-Flux-Request added).
pub async fn call_raw(
    router: &Router,
    method: Method,
    uri: &str,
    who: Option<Identity>,
    headers: &[(&str, &str)],
    body: Option<serde_json::Value>,
) -> Resp {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).expect("json"))
        }
        None => Body::empty(),
    };
    let mut req = req.body(body).expect("request");
    let path = if who.is_some() {
        NetPath::Localhost
    } else {
        NetPath::Lan
    };
    req.extensions_mut().insert(who);
    req.extensions_mut().insert(ConnInfo {
        path,
        remote_addr: None,
        tailnet_name: None,
        grants: Vec::new(),
    });
    let resp = router.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    Resp {
        status,
        headers,
        body,
    }
}

pub async fn get(router: &Router, uri: &str) -> Resp {
    call(router, Method::GET, uri, Some(owner()), &[], None).await
}

pub async fn post(router: &Router, uri: &str, body: serde_json::Value) -> Resp {
    call(router, Method::POST, uri, Some(owner()), &[], Some(body)).await
}

/// Chrome desktop capabilities (flux-hls preset), as JSON.
pub fn chrome_caps() -> serde_json::Value {
    serde_json::to_value(flux_hls::presets::chrome_desktop()).expect("caps")
}

pub fn safari_caps() -> serde_json::Value {
    serde_json::to_value(flux_hls::presets::safari_macos()).expect("caps")
}

/// Waits (polling) until `f` returns `Some`.
pub async fn eventually<T>(timeout_s: u64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_s);
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if std::time::Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

pub type Shared = Arc<flux_server::state::AppState>;
