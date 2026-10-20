//! On-disk response cache with a hard TMDB ToS age cap.
//!
//! TMDB API Terms §1.C: do not "cache, for longer than 6 months, any
//! information obtained through or from TMDB". Every entry records when it was
//! fetched; reads past `max_age` miss (and delete the file), and `max_age` can
//! never be configured above [`TMDB_MAX_CACHE_AGE`] (180 days, safely < 6 months).
//!
//! Layout: `<dir>/<2 hex>/<32 hex>.json`, each file a one-line JSON header
//! (`{"v":1,"fetched_at":…,"key":…}`) followed by the raw response body, so the
//! body is stored byte-for-byte and never re-encoded.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::util::{hash_hex, unix_now};

/// The longest flux will ever keep TMDB content: 180 days (< 6 months, ToS §1.C).
pub const TMDB_MAX_CACHE_AGE: Duration = Duration::from_secs(180 * 86_400);

/// Disk cache for provider responses.
#[derive(Debug, Clone)]
pub struct DiskCache {
    dir: PathBuf,
    max_age: Duration,
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    v: u8,
    fetched_at: u64,
    key: String,
}

/// What [`DiskCache::purge_expired`] removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PurgeStats {
    pub kept: u64,
    pub removed: u64,
}

impl DiskCache {
    /// A cache rooted at `dir` whose entries expire after `max_age`, clamped to
    /// [`TMDB_MAX_CACHE_AGE`].
    pub fn new(dir: impl Into<PathBuf>, max_age: Duration) -> Self {
        DiskCache {
            dir: dir.into(),
            max_age: max_age.min(TMDB_MAX_CACHE_AGE),
        }
    }

    /// The effective maximum age (after the ToS clamp).
    pub fn max_age(&self) -> Duration {
        self.max_age
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let h = hash_hex(key.as_bytes());
        self.dir.join(&h[..2]).join(format!("{h}.json"))
    }

    /// The cached body for `key`, if present and younger than `max_age`.
    pub async fn get(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.path_for(key);
        let data = tokio::fs::read(&path).await.ok()?;
        let nl = data.iter().position(|&b| b == b'\n')?;
        let header: Header = serde_json::from_slice(&data[..nl]).ok()?;
        // A different key with the same 128-bit hash is astronomically unlikely, but cheap to rule out.
        if header.key != key {
            return None;
        }
        let age = unix_now().saturating_sub(header.fetched_at);
        if age > self.max_age.as_secs() {
            let _ = tokio::fs::remove_file(&path).await;
            return None;
        }
        Some(data[nl + 1..].to_vec())
    }

    /// Store `body` for `key` (atomic: write to a temp file, then rename).
    pub async fn put(&self, key: &str, body: &[u8]) -> io::Result<()> {
        self.put_at(key, body, unix_now()).await
    }

    /// Store with an explicit fetch time (tests and imports).
    pub async fn put_at(&self, key: &str, body: &[u8], fetched_at: u64) -> io::Result<()> {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let header = serde_json::to_vec(&Header {
            v: 1,
            fetched_at,
            key: key.to_owned(),
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut buf = Vec::with_capacity(header.len() + 1 + body.len());
        buf.extend_from_slice(&header);
        buf.push(b'\n');
        buf.extend_from_slice(body);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        tokio::fs::write(&tmp, &buf).await?;
        tokio::fs::rename(&tmp, &path).await
    }

    /// Delete every entry older than `max_age`. Run daily; blocking I/O, so call
    /// it from `spawn_blocking` or a maintenance thread.
    pub fn purge_expired(&self) -> io::Result<PurgeStats> {
        let mut stats = PurgeStats::default();
        let now = unix_now();
        let Ok(shards) = std::fs::read_dir(&self.dir) else {
            return Ok(stats); // nothing cached yet
        };
        for shard in shards.flatten() {
            if !shard.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())?.flatten() {
                let path = entry.path();
                let fresh = std::fs::read(&path).ok().and_then(|data| {
                    let nl = data.iter().position(|&b| b == b'\n')?;
                    let h: Header = serde_json::from_slice(&data[..nl]).ok()?;
                    Some(now.saturating_sub(h.fetched_at) <= self.max_age.as_secs())
                });
                if fresh == Some(true) {
                    stats.kept += 1;
                } else {
                    std::fs::remove_file(&path)?;
                    stats.removed += 1;
                }
            }
        }
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_expiry_and_tos_clamp() {
        let dir = tempfile::tempdir().unwrap();
        // Asking for a year is clamped to 180 days.
        let cache = DiskCache::new(dir.path(), Duration::from_secs(365 * 86_400));
        assert_eq!(cache.max_age(), TMDB_MAX_CACHE_AGE);

        cache.put("/movie/603", b"{\"id\":603}").await.unwrap();
        assert_eq!(
            cache.get("/movie/603").await.as_deref(),
            Some(&b"{\"id\":603}"[..])
        );
        assert!(cache.get("/movie/604").await.is_none());

        // An entry fetched 181 days ago is expired and removed on read.
        let old = unix_now() - 181 * 86_400;
        cache.put_at("/movie/1", b"{}", old).await.unwrap();
        cache.put_at("/movie/2", b"{}", old).await.unwrap();
        assert!(cache.get("/movie/1").await.is_none());
        let stats = cache.purge_expired().unwrap();
        assert_eq!(stats, PurgeStats { kept: 1, removed: 1 });
    }
}
