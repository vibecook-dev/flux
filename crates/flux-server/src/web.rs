//! The web client (`web/dist`, built with `VITE_FLUX_API=/ pnpm build`).
//!
//! - Hashed assets (`/assets/*`) are `immutable`; everything else revalidates.
//! - Unknown non-API paths get `index.html` (client-side routing).
//! - `index.html` carries a strict Content-Security-Policy whose `script-src`
//!   allows exactly the inline theme script, by its SHA-256 (computed when
//!   the UI is loaded, so a rebuilt `index.html` stays covered).
//! - Text assets are gzipped once on first request and kept in memory.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::state::AppState;

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const REVALIDATE: &str = "no-cache";

/// The loaded web client.
pub struct WebUi {
    root: PathBuf,
    index: Bytes,
    csp: String,
    gz: Mutex<HashMap<PathBuf, Option<Bytes>>>,
}

impl WebUi {
    /// Loads `dir/index.html`; `None` if there is no built client there.
    pub fn load(dir: &Path) -> Option<WebUi> {
        let root = dir.canonicalize().ok()?;
        let index = std::fs::read(root.join("index.html")).ok()?;
        let html = String::from_utf8_lossy(&index);
        let csp = csp_for(&html);
        Some(WebUi {
            root,
            index: Bytes::from(index),
            csp,
            gz: Mutex::new(HashMap::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn csp(&self) -> &str {
        &self.csp
    }

    fn gzipped(&self, path: &Path, raw: &[u8]) -> Option<Bytes> {
        let mut map = self.gz.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(hit) = map.get(path) {
            return hit.clone();
        }
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
        let gz = enc
            .write_all(raw)
            .ok()
            .and_then(|_| enc.finish().ok())
            .filter(|g| g.len() + 64 < raw.len())
            .map(Bytes::from);
        map.insert(path.to_path_buf(), gz.clone());
        gz
    }
}

/// Inline `<script>` bodies (no `src`) of an HTML document.
pub fn inline_scripts(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("<script") {
        rest = &rest[i..];
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[..gt];
        let body_start = gt + 1;
        let Some(end) = rest[body_start..].find("</script>") else {
            break;
        };
        if !tag.contains(" src=") {
            out.push(&rest[body_start..body_start + end]);
        }
        rest = &rest[body_start + end..];
    }
    out
}

/// The CSP for `index.html`: same-origin everything, the inline theme
/// script by hash, `blob:` for MSE media and hls.js workers, `data:` for the
/// empty WebVTT `<track>` elements hls.js creates.
pub fn csp_for(html: &str) -> String {
    let hashes: Vec<String> = inline_scripts(html)
        .into_iter()
        .map(|s| {
            let digest = Sha256::digest(s.as_bytes());
            format!(
                "'sha256-{}'",
                base64::engine::general_purpose::STANDARD.encode(digest)
            )
        })
        .collect();
    let mut script = String::from("'self'");
    for h in &hashes {
        script.push(' ');
        script.push_str(h);
    }
    format!(
        "default-src 'self'; script-src {script}; style-src 'self'; img-src 'self' data: blob:; \
         media-src 'self' blob: data:; connect-src 'self'; font-src 'self'; worker-src 'self' blob:; \
         manifest-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'"
    )
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn compressible(ctype: &str) -> bool {
    ctype.starts_with("text/") || ctype.starts_with("application/json") || ctype == "image/svg+xml"
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|e| e.trim().starts_with("gzip")))
}

/// A path under the UI root, or `None` for anything that tries to leave it.
fn resolve(root: &Path, uri_path: &str) -> Option<PathBuf> {
    let rel = uri_path.trim_start_matches('/');
    let mut out = root.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn html_response(ui: &WebUi, headers: &HeaderMap) -> Response {
    let mut resp = file_response(ui, Path::new("index.html"), ui.index.clone(), REVALIDATE, headers);
    if let Ok(v) = HeaderValue::from_str(&ui.csp) {
        resp.headers_mut().insert(header::CONTENT_SECURITY_POLICY, v);
    }
    let h = resp.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

fn file_response(ui: &WebUi, path: &Path, raw: Bytes, cache: &'static str, headers: &HeaderMap) -> Response {
    let ctype = content_type(path);
    let etag = format!("\"{:016x}\"", xxhash_rust::xxh3::xxh3_64(&raw));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        if let Ok(v) = HeaderValue::from_str(&etag) {
            r.headers_mut().insert(header::ETAG, v);
        }
        return r;
    }
    let (body, gzip) = if compressible(ctype) && accepts_gzip(headers) {
        match ui.gzipped(path, &raw) {
            Some(gz) => (gz, true),
            None => (raw, false),
        }
    } else {
        (raw, false)
    };
    let mut b = Response::builder()
        .header(header::CONTENT_TYPE, ctype)
        .header(header::CACHE_CONTROL, cache)
        .header(header::ETAG, etag)
        .header(header::VARY, "accept-encoding")
        .header(header::CONTENT_LENGTH, body.len());
    if gzip {
        b = b.header(header::CONTENT_ENCODING, "gzip");
    }
    b.body(Body::from(body)).unwrap_or_default()
}

const NO_UI: &str = "<!doctype html><meta charset=utf-8><title>flux</title>\
<body style=\"font:16px system-ui;margin:3em;max-width:40em\"><h1>flux is running</h1>\
<p>The web client isn't built or wasn't found. Build it with \
<code>cd web &amp;&amp; VITE_FLUX_API=/ pnpm build</code> and start the server with \
<code>--web-dir web/dist</code>.</p><p>The API is at <code>/api/v1/</code>.</p>";

/// Everything the router doesn't match: API 404s, the UI and its files.
pub async fn fallback(
    State(st): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let path = uri.path();
    if path.starts_with("/api/") || path.starts_with("/media/") || path == "/api" || path == "/media" {
        return crate::error::ApiError::not_found(format!("No route {path}")).into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(ui) = st.web.as_ref() else {
        return ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], NO_UI).into_response();
    };
    if path == "/" || path == "/index.html" {
        return html_response(ui, &headers);
    }
    let Some(file) = resolve(&ui.root, path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let is_asset = path.starts_with("/assets/");
    let read = tokio::fs::read(&file).await;
    match read {
        Ok(raw) => {
            let cache = if is_asset { IMMUTABLE } else { REVALIDATE };
            file_response(ui, &file, Bytes::from(raw), cache, &headers)
        }
        // A missing hashed asset is a real 404 (stale tab after a rebuild),
        // not the SPA shell.
        Err(_) if is_asset || path.rsplit('/').next().is_some_and(|l| l.contains('.')) => {
            StatusCode::NOT_FOUND.into_response()
        }
        Err(_) => html_response(ui, &headers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_hashes_exactly_the_inline_scripts() {
        let html = "<head><script>\n  var a = 1;\n</script><script type=\"module\" crossorigin src=\"/assets/x.js\"></script></head>";
        let scripts = inline_scripts(html);
        assert_eq!(scripts, ["\n  var a = 1;\n"]);
        let csp = csp_for(html);
        let expected = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(b"\n  var a = 1;\n"));
        assert!(
            csp.contains(&format!("script-src 'self' 'sha256-{expected}';")),
            "{csp}"
        );
        assert!(!csp.contains("unsafe-inline"));
        // hls.js gives the <track> elements it creates `src = "data:,WEBVTT"`
        // (an empty cue list); blocked, Chrome logs a CSP error on playback.
        assert!(csp.contains("media-src 'self' blob: data:;"), "{csp}");
    }

    #[test]
    fn paths_cannot_escape_the_root() {
        let root = Path::new("/srv/web");
        assert_eq!(
            resolve(root, "/assets/a.js"),
            Some(PathBuf::from("/srv/web/assets/a.js"))
        );
        assert_eq!(resolve(root, "/../etc/passwd"), None);
        assert_eq!(resolve(root, "/assets/../../x"), None);
    }
}
