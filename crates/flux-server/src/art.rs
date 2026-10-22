//! `GET /api/v1/art/{id}?w=`: artwork, resized on demand and cached on disk.
//!
//! Art rows never change and their ids are never reused (new art gets a new
//! id), so responses are `immutable` with a strong ETag. Sources: a local
//! file (frame posters, local art) or a provider URL, fetched once into the
//! cache.
//!
//! flux's own images live in `<data dir>/art/` and the catalog stores them
//! relative to the data dir (`art/<file>`), so a data dir that is moved or
//! copied keeps serving them. Catalogs up to schema v5 stored the absolute
//! path of the data dir they were made in; [`ArtStore::candidates`] reads
//! such a path from this data dir's `art` folder first.

use std::collections::{HashMap, HashSet};
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
    /// The data dir, which stored paths are relative to.
    data_dir: PathBuf,
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
            data_dir: data_dir.to_path_buf(),
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
            for file in self.candidates(p) {
                if let Ok(b) = tokio::fs::read(&file).await {
                    return Ok(b);
                }
            }
            return Err(ApiError::not_found("This image is gone"));
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

impl ArtStore {
    /// The path the catalog stores for an image written to `art/<name>`:
    /// relative to the data dir, with `/` on every platform.
    pub fn stored_path(name: &str) -> String {
        format!("{ART_DIR}/{name}")
    }

    /// Where a stored path may be on disk, in the order to try:
    /// - relative (`art/<file>`): under this data dir;
    /// - absolute, in a folder named `art` (a catalog from before v6, made
    ///   in another data dir): this data dir's copy first, so a moved or
    ///   copied data dir serves its own files rather than the old folder's,
    ///   then the path as stored;
    /// - anything else (local art next to the media): as stored.
    pub fn candidates(&self, stored: &str) -> Vec<PathBuf> {
        if !looks_absolute(stored) {
            return vec![self.data_dir.join(stored)];
        }
        let mut out = Vec::with_capacity(2);
        if let Some(name) = name_in_art_folder(stored) {
            out.push(self.dir.join(name));
        }
        out.push(PathBuf::from(stored));
        out
    }

    /// The file of a stored path that this store owns and may delete: an
    /// image in this data dir's `art` folder. Never a file of another data
    /// dir (a legacy absolute path) or next to the media.
    pub fn owned_file(&self, stored: &str) -> Option<PathBuf> {
        let name = stored.strip_prefix(ART_DIR)?.strip_prefix('/')?;
        is_art_file_name(name).then(|| self.dir.join(name))
    }

    /// Delete the files of image rows the catalog deleted: the ones this
    /// store owns ([`ArtStore::owned_file`]) and the cached copies of
    /// `ids`. Blocking.
    pub fn remove(&self, stored: &[String], ids: &[Id]) {
        for p in stored.iter().filter_map(|s| self.owned_file(s)) {
            let _ = std::fs::remove_file(p);
        }
        for id in ids {
            let _ = std::fs::remove_file(self.cache_dir.join(format!("{id}-src")));
            for w in BUCKETS {
                let _ = std::fs::remove_file(self.cache_dir.join(format!("{id}-w{w}.jpg")));
            }
        }
    }

    /// Delete what no image row refers to any more (rows that went with
    /// their item, or an interrupted write): images in `art/` named as flux
    /// names them, and cached copies of ids that no longer exist. `rows`:
    /// every art row's id and stored path ([`flux_catalog::read::art_paths`]).
    /// Blocking; run before anything writes art. Returns the files deleted.
    pub fn sweep(&self, rows: &[(Id, Option<String>)]) -> usize {
        let ids: HashSet<Id> = rows.iter().map(|(id, _)| *id).collect();
        let names: HashSet<&str> = rows
            .iter()
            .filter_map(|(_, p)| p.as_deref())
            .filter_map(|p| {
                p.strip_prefix(ART_DIR)
                    .and_then(|r| r.strip_prefix('/'))
                    .or_else(|| name_in_art_folder(p))
            })
            .collect();
        let mut removed = 0;
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if is_art_file_name(name) && !names.contains(name) && std::fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
        for e in std::fs::read_dir(&self.cache_dir).into_iter().flatten().flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            let id: Option<Id> = name.split_once('-').and_then(|(id, _)| id.parse().ok());
            let stale = name.ends_with(".tmp") || id.is_some_and(|id| !ids.contains(&id));
            if stale && std::fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

/// The folder of flux's own images in the data dir.
const ART_DIR: &str = "art";

/// An absolute path in any platform's spelling (`/…`, `\…`, `C:…`), so a
/// catalog made on another platform is read the same way.
fn looks_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || p.starts_with('\\') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// The file name of a path whose folder is named `art` (any case, either
/// separator).
fn name_in_art_folder(p: &str) -> Option<&str> {
    let (dir, name) = p.rsplit_once(['/', '\\'])?;
    let folder = dir.rsplit(['/', '\\']).next()?;
    (folder.eq_ignore_ascii_case(ART_DIR) && !name.is_empty()).then_some(name)
}

/// A name flux gives its images: `<item>-<kind>-<8 hex digits>.jpg`.
fn is_art_file_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jpg") else {
        return false;
    };
    let mut parts = stem.splitn(3, '-');
    let (Some(item), Some(kind), Some(hash)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !item.is_empty()
        && item.bytes().all(|b| b.is_ascii_digit())
        && !kind.is_empty()
        && kind.bytes().all(|b| b.is_ascii_lowercase())
        && hash.len() == 8
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
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
    fn stored_paths_are_found_in_this_data_dir() {
        let d = tempfile::tempdir().unwrap();
        let store = ArtStore::new(&d.path().join("data"));
        let data = d.path().join("data");
        assert_eq!(
            ArtStore::stored_path("7-poster-0a1b2c3d.jpg"),
            "art/7-poster-0a1b2c3d.jpg"
        );
        assert_eq!(
            store.candidates("art/7-poster-0a1b2c3d.jpg"),
            [data.join("art/7-poster-0a1b2c3d.jpg")]
        );
        // A legacy absolute path of another data dir: this one's copy first.
        assert_eq!(
            store.candidates(r"D:\old data\art\7-poster-0a1b2c3d.jpg"),
            [
                data.join("art").join("7-poster-0a1b2c3d.jpg"),
                PathBuf::from(r"D:\old data\art\7-poster-0a1b2c3d.jpg")
            ]
        );
        assert_eq!(
            store.candidates("/srv/old/art/7-thumb-0a1b2c3d.jpg"),
            [
                data.join("art").join("7-thumb-0a1b2c3d.jpg"),
                PathBuf::from("/srv/old/art/7-thumb-0a1b2c3d.jpg")
            ]
        );
        // Local art next to the media is read where it is.
        assert_eq!(
            store.candidates("/media/Film (2001)/poster.jpg"),
            [PathBuf::from("/media/Film (2001)/poster.jpg")]
        );
        // Only this data dir's own images are ever deleted.
        assert_eq!(
            store.owned_file("art/7-poster-0a1b2c3d.jpg"),
            Some(data.join("art").join("7-poster-0a1b2c3d.jpg"))
        );
        assert_eq!(store.owned_file(r"D:\old data\art\7-poster-0a1b2c3d.jpg"), None);
        assert_eq!(store.owned_file("art/../catalog.db"), None);
        assert_eq!(store.owned_file("/media/Film (2001)/poster.jpg"), None);
    }

    #[test]
    fn sweep_deletes_only_unreferenced_flux_images_and_cache() {
        let d = tempfile::tempdir().unwrap();
        let store = ArtStore::new(d.path());
        let write = |p: PathBuf| std::fs::write(p, b"x").unwrap();
        write(store.dir.join("1-poster-0000000a.jpg")); // referenced (relative)
        write(store.dir.join("2-poster-0000000b.jpg")); // referenced (legacy absolute)
        write(store.dir.join("3-poster-0000000c.jpg")); // its row is gone
        write(store.dir.join("notes.txt")); // not flux's
        write(store.cache_dir.join("10-w320.jpg"));
        write(store.cache_dir.join("11-src"));
        write(store.cache_dir.join("10-w640.tmp"));
        let rows = vec![
            (10, Some("art/1-poster-0000000a.jpg".to_string())),
            (12, Some(r"D:\old\art\2-poster-0000000b.jpg".to_string())),
        ];
        assert_eq!(store.sweep(&rows), 3);
        assert!(store.dir.join("1-poster-0000000a.jpg").exists());
        assert!(store.dir.join("2-poster-0000000b.jpg").exists());
        assert!(!store.dir.join("3-poster-0000000c.jpg").exists());
        assert!(store.dir.join("notes.txt").exists());
        assert!(store.cache_dir.join("10-w320.jpg").exists());
        assert!(!store.cache_dir.join("11-src").exists());
        assert!(!store.cache_dir.join("10-w640.tmp").exists());
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
