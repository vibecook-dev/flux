//! `/media/…`: HLS resources of a plan and the original file.
//!
//! - `/media/{plan_id}/{resource}` → `HlsResource::parse` → `HlsEngine::serve`,
//!   streamed with `Content-Length` and the engine's `Cache-Control`
//!   (immutable for init/segments), plus `X-Flux-Ms` (server production
//!   time, as `hls-serve` reports it).
//! - `/media/{asset_id}/file` → the file itself with `Range`, `If-Range`,
//!   `ETag`/`Last-Modified`, streamed in 1 MiB positioned reads with one read
//!   ahead (memory per stream ≤ ~3 MiB, no blocking thread parked per stream).
//!   Only a file inside its library, or the very file the scan recorded
//!   behind a link that leaves it, is served ([`play::open_contained`]).
//!
//! A client that stops reading is dropped by flux-net's write timeout.

use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use flux_core::api::Id;
use flux_core::Container;
use flux_hls::{HlsEngine, HlsResource, WebVttBody};
use futures::StreamExt;

use crate::error::{ApiError, ApiResult};
use crate::play;
use crate::state::AppState;

/// Bytes per read when streaming a file or a segment's source ranges.
const CHUNK: usize = 1 << 20;

/// `GET /media/{id}/{*rest}`.
pub async fn media(
    State(st): State<Arc<AppState>>,
    Path((id, rest)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    st.activity.touch_media();
    let result = if rest == "file" {
        match id.parse::<Id>() {
            Ok(asset_id) => serve_file(&st, asset_id, &headers).await,
            Err(_) => Err(ApiError::not_found("No such file")),
        }
    } else {
        serve_hls(&st, &id, &rest, &headers).await
    };
    result.unwrap_or_else(IntoResponse::into_response)
}

fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "*" || v.split(',').any(|t| t.trim().trim_start_matches("W/") == etag))
}

async fn serve_hls(st: &AppState, plan_id: &str, rest: &str, headers: &HeaderMap) -> ApiResult<Response> {
    let entry = st.play.plan_entry(plan_id).await?;
    let res = HlsResource::parse(rest).ok_or_else(|| ApiError::not_found(format!("No resource {rest}")))?;
    let immutable = matches!(
        res,
        HlsResource::Init(_) | HlsResource::Segment(..) | HlsResource::WebVtt(_)
    );
    // The plan id pins (asset, file identity, recipe), so the path is a
    // strong validator, except for a video transcode's init and segments:
    // equivalent whoever asks, but not byte-identical across encoder runs
    // (`HlsEngine::is_deterministic`, review PB-11), hence a weak one.
    let etag = if HlsEngine::is_deterministic(&entry.plan, &res) {
        format!("\"{plan_id}/{rest}\"")
    } else {
        format!("W/\"{plan_id}/{rest}\"")
    };
    if immutable && etag_matches(headers, etag.trim_start_matches("W/")) {
        return Ok(not_modified(&etag, "public, max-age=31536000, immutable"));
    }
    let asset = st.play.asset_for_plan(st, &entry).await?;
    let t0 = Instant::now();
    if let HlsResource::WebVtt(t) = res {
        // A subtitle file is read only from inside the asset's library (a
        // file replaced by a link since the scan is refused, review SEC-2).
        let file = entry
            .plan
            .subtitles
            .iter()
            .find(|s| s.track == t)
            .and_then(|s| s.file.as_ref());
        if let Some(f) = file {
            let (_, roots) = play::locate(st, entry.asset_id).await?;
            let path = PathBuf::from(&f.path);
            tokio::task::spawn_blocking(move || play::contained(&path, &roots))
                .await
                .map_err(|e| ApiError::internal(e.to_string()))??;
        }
        // Direct play still gets WebVTT sidecars.
        let body = st.engine.plan_webvtt_body(&asset, &entry.plan, t).await?;
        return webvtt_response(body, &etag, t0);
    }
    if entry.is_direct() {
        return Err(ApiError::not_found("This stream plays the file directly"));
    }
    let r = st.engine.serve(&asset, &entry.plan, res).await?;
    let ms = t0.elapsed().as_secs_f32() * 1000.0;
    let len = r.body.len();
    if matches!(res, HlsResource::Segment(..)) {
        entry.stats.record(ms, len);
    }
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, r.content_type)
        .header(header::CONTENT_LENGTH, len)
        .header(header::CACHE_CONTROL, r.cache_control)
        .header("x-flux-ms", format!("{ms:.3}"));
    if immutable {
        resp = resp.header(header::ETAG, &etag);
    }
    // In-memory parts (headers, playlists, cached transcodes) go out as they
    // are; source ranges are read in CHUNK batches as the socket drains. A
    // read that fails after the headers (a file truncated or yanked since
    // the segment was built) can only abort the response: say why in the log.
    let what = format!("{plan_id}/{rest}");
    let body = r.body.into_stream(CHUNK).inspect(move |item| {
        if let Err(e) = item {
            tracing::warn!(error = %e, resource = %what, "segment body failed mid-stream");
        }
    });
    resp.body(Body::from_stream(body))
        .map_err(|e| ApiError::internal(e.to_string()))
}

/// A WebVTT sidecar: with its length when it was extracted before, else
/// chunked, cue by cue as the track is read (review PB-7).
fn webvtt_response(body: WebVttBody, etag: &str, t0: Instant) -> ApiResult<Response> {
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/vtt; charset=utf-8")
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .header(header::ETAG, etag);
    let resp = match body {
        WebVttBody::Whole(doc) => resp
            .header(header::CONTENT_LENGTH, doc.len())
            .header("x-flux-ms", format!("{:.3}", t0.elapsed().as_secs_f32() * 1000.0))
            .body(Body::from(Bytes::from(doc.to_string()))),
        WebVttBody::Streaming(s) => resp.body(Body::from_stream(s.inspect(|item| {
            if let Err(e) = item {
                tracing::warn!(error = %e, "subtitle extraction failed mid-stream");
            }
        }))),
    };
    resp.map_err(|e| ApiError::internal(e.to_string()))
}

fn not_modified(etag: &str, cache_control: &'static str) -> Response {
    let mut r = StatusCode::NOT_MODIFIED.into_response();
    if let Ok(v) = HeaderValue::from_str(etag) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control));
    r
}

/// MIME type for direct play.
fn content_type(c: Option<Container>, path: &str) -> &'static str {
    match c {
        Some(Container::Matroska) => "video/x-matroska",
        Some(Container::WebM) => "video/webm",
        Some(Container::Mp4) => "video/mp4",
        Some(Container::Mov) => "video/quicktime",
        Some(Container::MpegTs | Container::M2ts) => "video/mp2t",
        Some(Container::Avi) => "video/x-msvideo",
        _ => match path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
            Some("mkv" | "mk3d") => "video/x-matroska",
            Some("webm") => "video/webm",
            Some("mp4" | "m4v") => "video/mp4",
            Some("mov" | "qt") => "video/quicktime",
            Some("ts" | "m2ts" | "mts") => "video/mp2t",
            Some("avi") => "video/x-msvideo",
            _ => "application/octet-stream",
        },
    }
}

/// A parsed `Range` header against a body of `len` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeReq {
    /// No usable range: send everything (also for multi-range requests).
    Full,
    /// Inclusive byte range.
    Part(u64, u64),
    /// Outside the body: 416.
    Unsatisfiable,
}

/// RFC 9110 §14.1.2, one range only (`bytes=a-b`, `bytes=a-`, `bytes=-n`).
pub fn parse_range(value: Option<&str>, len: u64) -> RangeReq {
    let Some(spec) = value.and_then(|v| v.trim().strip_prefix("bytes=")) else {
        return RangeReq::Full;
    };
    if spec.contains(',') {
        return RangeReq::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeReq::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // Suffix: the last n bytes.
        return match b.parse::<u64>() {
            Ok(0) => RangeReq::Unsatisfiable,
            Ok(n) if len > 0 => RangeReq::Part(len.saturating_sub(n), len - 1),
            Ok(_) => RangeReq::Unsatisfiable,
            Err(_) => RangeReq::Full,
        };
    }
    let Ok(start) = a.parse::<u64>() else {
        return RangeReq::Full;
    };
    if start >= len {
        return RangeReq::Unsatisfiable;
    }
    let end = if b.is_empty() {
        len - 1
    } else {
        match b.parse::<u64>() {
            Ok(e) if e >= start => e.min(len - 1),
            _ => return RangeReq::Full,
        }
    };
    RangeReq::Part(start, end)
}

async fn serve_file(st: &AppState, asset_id: Id, headers: &HeaderMap) -> ApiResult<Response> {
    let t0 = Instant::now();
    let (loc, roots) = play::locate(st, asset_id).await?;
    let path = PathBuf::from(&loc.path);
    let rec = play::Recorded::of(&loc);
    let (file, meta) = tokio::task::spawn_blocking(move || play::open_contained(&path, &roots, &rec))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
    let len = meta.len();
    let mtime = meta.modified().ok();
    let mtime_ns = mtime
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let etag = format!("\"{len:x}-{mtime_ns:x}\"");
    let last_modified = mtime.map(httpdate::fmt_http_date);
    const CC: &str = "private, max-age=3600";
    if etag_matches(headers, &etag) {
        return Ok(not_modified(&etag, CC));
    }
    // If-Range: the range only applies to the representation it names.
    let if_range_ok = match headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(v) if v.starts_with('"') || v.starts_with("W/") => v == etag,
        Some(v) => last_modified.as_deref() == Some(v),
    };
    let range = if if_range_ok {
        parse_range(headers.get(header::RANGE).and_then(|v| v.to_str().ok()), len)
    } else {
        RangeReq::Full
    };
    let ctype = content_type(loc.container, &loc.path);
    let mut resp = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, CC)
        .header(header::CONTENT_TYPE, ctype);
    if let Some(lm) = &last_modified {
        resp = resp.header(header::LAST_MODIFIED, lm);
    }
    let (start, end, status) = match range {
        RangeReq::Unsatisfiable => {
            return resp
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .header(header::CONTENT_LENGTH, 0)
                .body(Body::empty())
                .map_err(|e| ApiError::internal(e.to_string()));
        }
        RangeReq::Part(s, e) => (s, e, StatusCode::PARTIAL_CONTENT),
        RangeReq::Full if len == 0 => (0, 0, StatusCode::OK),
        RangeReq::Full => (0, len - 1, StatusCode::OK),
    };
    let total = if len == 0 { 0 } else { end - start + 1 };
    if status == StatusCode::PARTIAL_CONTENT {
        resp = resp.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
    }
    // One "segment" per response; bytes are counted as they are sent.
    let stats = st.play.direct_stats(asset_id);
    stats.record(t0.elapsed().as_secs_f32() * 1000.0, 0);
    resp.status(status)
        .header(header::CONTENT_LENGTH, total)
        .body(Body::from_stream(file_stream(
            Arc::new(file),
            start,
            total,
            stats,
        )))
        .map_err(|e| ApiError::internal(e.to_string()))
}

/// Streams `[start, start + total)` of `file` in `CHUNK` positioned reads on
/// the blocking pool, one read ahead of the socket.
fn file_stream(
    file: Arc<File>,
    start: u64,
    total: u64,
    stats: Arc<crate::play::StreamStats>,
) -> impl futures::Stream<Item = io::Result<Bytes>> + Send + 'static {
    let chunks = total.div_ceil(CHUNK as u64);
    futures::stream::iter(0..chunks)
        .map(move |i| {
            let file = file.clone();
            let off = start + i * CHUNK as u64;
            let n = (total - i * CHUNK as u64).min(CHUNK as u64) as usize;
            async move {
                tokio::task::spawn_blocking(move || read_exact_at(&file, off, n))
                    .await
                    .unwrap_or_else(|e| Err(io::Error::other(e)))
            }
        })
        .buffered(2)
        .inspect(move |r| {
            if let Ok(b) = r {
                stats.add_bytes(b.len() as u64);
            }
        })
}

fn read_exact_at(file: &File, off: u64, n: usize) -> io::Result<Bytes> {
    let mut buf = vec![0u8; n];
    let mut done = 0;
    while done < n {
        let got = pread(file, &mut buf[done..], off + done as u64)?;
        if got == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file shrank while streaming",
            ));
        }
        done += got;
    }
    Ok(Bytes::from(buf))
}

#[cfg(unix)]
fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, off)
}

#[cfg(windows)]
fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, off)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), RangeReq::Full);
        assert_eq!(parse_range(Some("bytes=0-9"), 100), RangeReq::Part(0, 9));
        assert_eq!(parse_range(Some("bytes=90-"), 100), RangeReq::Part(90, 99));
        assert_eq!(parse_range(Some("bytes=90-500"), 100), RangeReq::Part(90, 99));
        assert_eq!(parse_range(Some("bytes=-10"), 100), RangeReq::Part(90, 99));
        assert_eq!(parse_range(Some("bytes=-500"), 100), RangeReq::Part(0, 99));
        assert_eq!(parse_range(Some("bytes=100-"), 100), RangeReq::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), 100), RangeReq::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=5-1"), 100), RangeReq::Full);
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), RangeReq::Full);
        assert_eq!(parse_range(Some("items=0-1"), 100), RangeReq::Full);
        assert_eq!(parse_range(Some("bytes=0-"), 0), RangeReq::Unsatisfiable);
    }

    #[test]
    fn etags() {
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"a\", W/\"b\""));
        assert!(etag_matches(&h, "\"b\""));
        assert!(!etag_matches(&h, "\"c\""));
    }
}
