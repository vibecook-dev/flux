//! `GET /api/v1/art/{id}?w=`: artwork, resized on demand and cached on disk.
//!
//! Art rows never change (new art gets a new id), so responses are
//! `immutable` with a strong ETag. Sources: a local file (frame posters,
//! local art) or a provider URL, fetched once into the cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use flux_catalog::read::{self, ArtRow};
use flux_core::api::Id;
use serde::Deserialize;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// Widths are rounded up to these so caches stay small (the web client asks
/// for exactly these).
const BUCKETS: [u32; 9] = [160, 240, 320, 480, 640, 960, 1280, 1920, 2560];

/// Where art lives on disk.
pub struct ArtStore {
    /// Generated art (frame posters …).
    pub dir: PathBuf,
    /// Resized copies and fetched provider images.
    cache_dir: PathBuf,
    http: reqwest::Client,
    inflight: Mutex<HashMap<(Id, u32), Gate>>,
}

/// Serialises work on one key (single flight).
type Gate = Arc<tokio::sync::Mutex<()>>;

impl ArtStore {
    pub fn new(data_dir: &Path) -> ArtStore {
        let dir = data_dir.join("art");
        let cache_dir = data_dir.join("art-cache");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::create_dir_all(&cache_dir);
        ArtStore {
            dir,
            cache_dir,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// The image bytes for `row` at width `w` (`0` = original) and their MIME type.
    pub async fn render(&self, row: &ArtRow, w: u32) -> ApiResult<(Vec<u8>, &'static str)> {
        let key = (row.id, w);
        let gate = self
            .inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(key)
            .or_default()
            .clone();
        let _g = gate.lock().await;
        let out = self.render_inner(row, w).await;
        self.inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
        out
    }

    async fn render_inner(&self, row: &ArtRow, w: u32) -> ApiResult<(Vec<u8>, &'static str)> {
        let resized = self.cache_dir.join(format!("{}-w{w}.jpg", row.id));
        if w > 0 {
            if let Ok(b) = tokio::fs::read(&resized).await {
                return Ok((b, "image/jpeg"));
            }
        }
        let original = self.original(row).await?;
        let mime = sniff(&original);
        let width = row.width.unwrap_or(u32::MAX);
        if w == 0 || w >= width {
            return Ok((original, mime));
        }
        let out = tokio::task::spawn_blocking(move || resize_jpeg(&original, w))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))??;
        let tmp = resized.with_extension("tmp");
        if tokio::fs::write(&tmp, &out).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &resized).await;
        }
        Ok((out, "image/jpeg"))
    }

    async fn original(&self, row: &ArtRow) -> ApiResult<Vec<u8>> {
        if let Some(p) = &row.path {
            return tokio::fs::read(p)
                .await
                .map_err(|_| ApiError::not_found("This image is gone"));
        }
        let Some(url) = &row.url else {
            return Err(ApiError::not_found("This image has no source"));
        };
        let cached = self.cache_dir.join(format!("{}-src", row.id));
        if let Ok(b) = tokio::fs::read(&cached).await {
            return Ok(b);
        }
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("Fetching the image failed: {e}")))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("Fetching the image failed: {e}")))?;
        let tmp = cached.with_extension("tmp");
        if tokio::fs::write(&tmp, &bytes).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &cached).await;
        }
        Ok(bytes.to_vec())
    }
}

fn sniff(b: &[u8]) -> &'static str {
    if b.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else if b.starts_with(b"\x89PNG") {
        "image/png"
    } else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

/// Downscale to width `w` (aspect kept) and encode as JPEG q82.
fn resize_jpeg(src: &[u8], w: u32) -> ApiResult<Vec<u8>> {
    let img = image::load_from_memory(src)
        .map_err(|e| ApiError::internal(format!("Decoding the image failed: {e}")))?;
    let h = ((img.height() as f64 * w as f64 / img.width().max(1) as f64).round() as u32).max(1);
    let small = img
        .resize_exact(w, h, image::imageops::FilterType::CatmullRom)
        .to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82)
        .encode_image(&small)
        .map_err(|e| ApiError::internal(format!("Encoding the image failed: {e}")))?;
    Ok(out)
}

/// The cache bucket for a requested width (`0` = original).
pub fn bucket(w: Option<u32>) -> u32 {
    match w {
        None | Some(0) => 0,
        Some(w) => BUCKETS.iter().copied().find(|b| *b >= w).unwrap_or(0),
    }
}

#[derive(Debug, Deserialize)]
pub struct ArtQuery {
    pub w: Option<u32>,
}

/// `GET /api/v1/art/{id}?w=`.
pub async fn get(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<Id>,
    query: Result<Query<ArtQuery>, QueryRejection>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let Query(q) = query?;
    let w = bucket(q.w);
    let etag = format!("\"art-{id}-{w}\"");
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    if fresh {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
        if let Ok(v) = HeaderValue::from_str(&etag) {
            r.headers_mut().insert(header::ETAG, v);
        }
        return Ok(r);
    }
    let row = st
        .catalog
        .read(move |c| read::art(c, id))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("No image {id}")))?;
    let (bytes, mime) = st.art.render(&row, w).await?;
    Ok((
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CACHE_CONTROL, IMMUTABLE.to_string()),
            (header::ETAG, etag),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_round_up_to_buckets() {
        assert_eq!(bucket(None), 0);
        assert_eq!(bucket(Some(1)), 160);
        assert_eq!(bucket(Some(300)), 320);
        assert_eq!(bucket(Some(2560)), 2560);
        assert_eq!(bucket(Some(5000)), 0);
    }

    #[test]
    fn resizes_and_sniffs() {
        let img = image::RgbImage::from_fn(64, 96, |x, y| image::Rgb([x as u8 * 4, y as u8 * 2, 128]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(sniff(&png), "image/png");
        let jpg = resize_jpeg(&png, 32).unwrap();
        assert_eq!(sniff(&jpg), "image/jpeg");
        let back = image::load_from_memory(&jpg).unwrap();
        assert_eq!((back.width(), back.height()), (32, 48));
    }
}
