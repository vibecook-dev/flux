//! Offline TMDB ID-export index: a compact, key-less candidate prefilter.
//!
//! TMDB publishes daily ID exports that need **no API key**
//! (`https://files.tmdb.org/p/exports/{movie_ids|tv_series_ids}_MM_DD_YYYY.json.gz`,
//! generated 07:00–08:00 UTC and kept ~3 months; before that hour today's file
//! is a 403, so [`download_latest`] walks back a few days). Each line is
//! `{"adult","id","original_title","popularity","video"}` (movies) or
//! `{"id","original_name","popularity"}` (TV). There is **no year and no
//! translated title**, so this index can only *propose* candidates; the
//! [`crate::matcher`] and TMDB `/search` decide (research/05 §3.3).
//!
//! # Layout (why it is small)
//!
//! The research prototype's `HashMap<String, Vec<u32>>` took 381 MB. Here all
//! distinct normalised titles live in one sorted string arena with `u32`
//! offsets, entries are 6-byte `(id, bfloat16 popularity)` columns grouped by title, and a
//! token → title-ordinal postings list supports fuzzy lookup by the query's
//! rarest words. Exact lookup is a binary search; fuzzy lookup scores at most a
//! few thousand titles with [`crate::normalize::title_similarity`].

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use flate2::read::MultiGzDecoder;
use serde::Deserialize;

use crate::normalize::{normalize_title, strip_article, title_similarity};
use crate::util::{civil_from_days, unix_now, utc_day};

/// Base URL of the daily exports (HTTPS; plain HTTP also works).
pub const EXPORT_BASE_URL: &str = "https://files.tmdb.org/p/exports";

/// Which export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportKind {
    Movie,
    Tv,
}

impl ExportKind {
    fn file_stem(self) -> &'static str {
        match self {
            ExportKind::Movie => "movie_ids",
            ExportKind::Tv => "tv_series_ids",
        }
    }
}

/// Errors building or downloading an export index.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("I/O error on {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("HTTP error downloading the TMDB export: {0}")]
    Http(#[from] reqwest::Error),
    #[error("no TMDB {kind:?} export found for the last {days} days (last status {status})")]
    NotPublished {
        kind: ExportKind,
        days: u32,
        status: u16,
    },
    #[error("the index is too large for 32-bit offsets")]
    TooLarge,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> ExportError + '_ {
    move |source| ExportError::Io {
        path: path.to_owned(),
        source,
    }
}

/// File name of the export for a UTC day (days since 1970-01-01).
pub fn export_file_name(kind: ExportKind, utc_day: u64) -> String {
    let (y, m, d) = civil_from_days(utc_day as i64);
    format!("{}_{m:02}_{d:02}_{y:04}.json.gz", kind.file_stem())
}

/// Full URL of the export for a UTC day.
pub fn export_url(kind: ExportKind, utc_day: u64) -> String {
    format!("{EXPORT_BASE_URL}/{}", export_file_name(kind, utc_day))
}

/// Download the newest available export into `dir` (skipping the download if
/// the file is already there) and return its path. Tries today (UTC) and up to
/// `max_days_back` earlier days: today's file only appears after ~08:00 UTC.
pub async fn download_latest(
    http: &reqwest::Client,
    dir: &Path,
    kind: ExportKind,
    max_days_back: u32,
) -> Result<PathBuf, ExportError> {
    download_latest_from(
        http,
        EXPORT_BASE_URL,
        dir,
        kind,
        max_days_back,
        utc_day(unix_now()),
    )
    .await
}

/// [`download_latest`] against an explicit base URL and "today" (for tests).
pub async fn download_latest_from(
    http: &reqwest::Client,
    base_url: &str,
    dir: &Path,
    kind: ExportKind,
    max_days_back: u32,
    today: u64,
) -> Result<PathBuf, ExportError> {
    tokio::fs::create_dir_all(dir).await.map_err(io_err(dir))?;
    let mut last_status = 0u16;
    for back in 0..=u64::from(max_days_back) {
        let name = export_file_name(kind, today.saturating_sub(back));
        let path = dir.join(&name);
        if tokio::fs::metadata(&path)
            .await
            .map(|m| m.len() > 0)
            .unwrap_or(false)
        {
            return Ok(path);
        }
        let url = format!("{}/{name}", base_url.trim_end_matches('/'));
        let mut resp = http.get(&url).timeout(Duration::from_secs(600)).send().await?;
        if !resp.status().is_success() {
            // 403 (S3 for "no such key") or 404: not generated yet / expired.
            last_status = resp.status().as_u16();
            continue;
        }
        let tmp = path.with_extension("partial");
        let mut file = tokio::fs::File::create(&tmp).await.map_err(io_err(&tmp))?;
        while let Some(chunk) = resp.chunk().await? {
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(io_err(&tmp))?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(io_err(&tmp))?;
        drop(file);
        tokio::fs::rename(&tmp, &path).await.map_err(io_err(&path))?;
        tracing::info!(%url, "downloaded TMDB export");
        return Ok(path);
    }
    Err(ExportError::NotPublished {
        kind,
        days: max_days_back + 1,
        status: last_status,
    })
}

/// One export row (borrowed where possible; titles with escapes allocate).
#[derive(Deserialize)]
struct Row<'a> {
    id: u32,
    #[serde(borrow, alias = "original_name")]
    original_title: Option<std::borrow::Cow<'a, str>>,
    #[serde(default)]
    popularity: Option<f32>,
    // Real exports contain the odd `"video": null`; treat null as false.
    #[serde(default)]
    adult: Option<bool>,
    #[serde(default)]
    video: Option<bool>,
}

/// One id under a normalised title.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct Entry {
    pub id: u32,
    pub popularity: f32,
}

/// How the export was filtered while building.
#[derive(Debug, Clone, Copy)]
pub struct BuildOptions {
    /// Skip `adult: true` rows (default true).
    pub skip_adult: bool,
    /// Skip `video: true` rows: direct-to-video compilations and clips (default true).
    pub skip_video: bool,
    /// Do not index tokens that occur in more titles than this (stop words like
    /// "the"); they are useless for candidate generation and cost memory.
    pub max_token_df: u32,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            skip_adult: true,
            skip_video: true,
            max_token_df: 20_000,
        }
    }
}

/// Build statistics.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct BuildStats {
    pub rows_read: u64,
    pub rows_indexed: u64,
    pub rows_skipped: u64,
    /// Lines that were not valid export JSON (skipped).
    pub rows_malformed: u64,
    pub distinct_titles: u64,
    pub tokens: u64,
    pub postings: u64,
}

/// How a candidate was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HitKind {
    /// Normalised title equal (possibly modulo a leading article).
    Exact,
    /// Found via the query's rarest tokens and scored by similarity.
    Fuzzy,
}

/// A prefilter candidate.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExportCandidate {
    pub id: u32,
    pub kind: ExportKind,
    /// The normalised `original_title`/`original_name` (the export has nothing else).
    pub title: String,
    pub popularity: f32,
    /// [`title_similarity`] between the normalised query and `title`.
    pub similarity: f32,
    pub hit: HitKind,
}

/// The compact index. See the module docs for the layout.
#[derive(Debug)]
pub struct ExportIndex {
    kind: ExportKind,
    /// Sorted, distinct normalised titles, concatenated.
    arena: String,
    /// `title_off[i]..title_off[i+1]` is title `i` in `arena`.
    title_off: Vec<u32>,
    /// `entry_off[i]..entry_off[i+1]` are title `i`'s entries (popularity-descending),
    /// stored as two columns: TMDB ids and bfloat16 popularity (8-bit mantissa,
    /// <0.4% error: plenty for a prior, and 2 bytes instead of 4).
    entry_off: Vec<u32>,
    ids: Vec<u32>,
    pops: Vec<u16>,
    /// Sorted distinct tokens, concatenated, with offsets.
    tok_arena: String,
    tok_off: Vec<u32>,
    /// `post_off[t]..post_off[t+1]` are the ordinals of titles containing token `t`
    /// (empty for stop tokens above `max_token_df`).
    post_off: Vec<u32>,
    postings: Vec<u32>,
    stats: BuildStats,
}

impl ExportIndex {
    /// Build from an export `.json.gz` file.
    pub fn build_from_gz(path: &Path, kind: ExportKind, opts: BuildOptions) -> Result<Self, ExportError> {
        let file = std::fs::File::open(path).map_err(io_err(path))?;
        // Exports are concatenated gzip members on some days; MultiGz handles both.
        let reader = BufReader::with_capacity(
            1 << 20,
            MultiGzDecoder::new(BufReader::with_capacity(1 << 20, file)),
        );
        Self::build_from_reader(reader, kind, opts)
    }

    /// Build from any reader of newline-delimited export JSON (uncompressed).
    pub fn build_from_reader(
        mut reader: impl BufRead,
        kind: ExportKind,
        opts: BuildOptions,
    ) -> Result<Self, ExportError> {
        let mut stats = BuildStats::default();
        // Pass 1: normalise every row into one scratch arena (no per-title allocation).
        let mut scratch = String::new();
        let mut rows: Vec<(u32, u32, Entry)> = Vec::new(); // (offset, len, entry)
        let mut line = String::new();
        let mut line_no = 0u64;
        loop {
            line.clear();
            let n = reader.read_line(&mut line).map_err(|source| ExportError::Io {
                path: PathBuf::from("<export>"),
                source,
            })?;
            if n == 0 {
                break;
            }
            line_no += 1;
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            stats.rows_read += 1;
            let row: Row<'_> = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(e) => {
                    // One bad line must not cost the whole index.
                    if stats.rows_malformed < 5 {
                        tracing::warn!(line = line_no, error = %e, "skipping malformed export line");
                    }
                    stats.rows_malformed += 1;
                    continue;
                }
            };
            if (opts.skip_adult && row.adult == Some(true)) || (opts.skip_video && row.video == Some(true)) {
                stats.rows_skipped += 1;
                continue;
            }
            let Some(title) = row.original_title else {
                stats.rows_skipped += 1;
                continue;
            };
            let norm = normalize_title(&title);
            if norm.is_empty() {
                stats.rows_skipped += 1;
                continue;
            }
            let off = u32::try_from(scratch.len()).map_err(|_| ExportError::TooLarge)?;
            scratch.push_str(&norm);
            rows.push((
                off,
                norm.len() as u32,
                Entry {
                    id: row.id,
                    popularity: row.popularity.unwrap_or(0.0),
                },
            ));
        }
        stats.rows_indexed = rows.len() as u64;

        // Sort by title, then popularity descending (so the first entry is the most popular).
        let key = |r: &(u32, u32, Entry)| &scratch[r.0 as usize..(r.0 + r.1) as usize];
        rows.sort_unstable_by(|a, b| {
            key(a)
                .cmp(key(b))
                .then_with(|| b.2.popularity.total_cmp(&a.2.popularity))
        });

        // Pass 2: dedupe titles into the final arena.
        let mut arena = String::with_capacity(scratch.len() / 2);
        let mut title_off = Vec::new();
        let mut entry_off = Vec::new();
        let mut ids = Vec::with_capacity(rows.len());
        let mut pops = Vec::with_capacity(rows.len());
        let mut prev: Option<&str> = None;
        for r in &rows {
            let t = key(r);
            if prev != Some(t) {
                title_off.push(u32::try_from(arena.len()).map_err(|_| ExportError::TooLarge)?);
                entry_off.push(ids.len() as u32);
                arena.push_str(t);
                prev = Some(t);
            }
            ids.push(r.2.id);
            pops.push(to_bf16(r.2.popularity));
        }
        title_off.push(arena.len() as u32);
        entry_off.push(ids.len() as u32);
        drop(rows);
        drop(scratch);
        arena.shrink_to_fit();
        title_off.shrink_to_fit();
        entry_off.shrink_to_fit();
        let n_titles = title_off.len() - 1;
        stats.distinct_titles = n_titles as u64;

        // Token index: count document frequencies, then fill postings (counting sort).
        let title_at = |i: usize| &arena[title_off[i] as usize..title_off[i + 1] as usize];
        let mut df: HashMap<&str, u32> = HashMap::with_capacity(n_titles / 2);
        for i in 0..n_titles {
            for tok in unique_tokens(title_at(i)) {
                *df.entry(tok).or_insert(0) += 1;
            }
        }
        let mut toks: Vec<(&str, u32)> = df.into_iter().collect();
        toks.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut tok_arena = String::new();
        let mut tok_off = Vec::with_capacity(toks.len() + 1);
        let mut post_off = Vec::with_capacity(toks.len() + 1);
        let mut total = 0u32;
        for (t, n) in &toks {
            tok_off.push(tok_arena.len() as u32);
            tok_arena.push_str(t);
            post_off.push(total);
            if *n <= opts.max_token_df {
                total += n;
            }
        }
        tok_off.push(tok_arena.len() as u32);
        post_off.push(total);
        let tok_id: HashMap<&str, (u32, u32)> = toks
            .iter()
            .enumerate()
            .map(|(i, (t, n))| (*t, (i as u32, *n)))
            .collect();
        let mut fill: Vec<u32> = post_off[..post_off.len() - 1].to_vec();
        let mut postings = vec![0u32; total as usize];
        for i in 0..n_titles {
            for tok in unique_tokens(title_at(i)) {
                let Some(&(t, df)) = tok_id.get(tok) else { continue };
                if df <= opts.max_token_df {
                    postings[fill[t as usize] as usize] = i as u32;
                    fill[t as usize] += 1;
                }
            }
        }
        drop(tok_id);
        stats.tokens = toks.len() as u64;
        stats.postings = postings.len() as u64;
        drop(toks);
        tok_arena.shrink_to_fit();

        Ok(ExportIndex {
            kind,
            arena,
            title_off,
            entry_off,
            ids,
            pops,
            tok_arena,
            tok_off,
            post_off,
            postings,
            stats,
        })
    }

    /// Which export this index was built from.
    pub fn kind(&self) -> ExportKind {
        self.kind
    }

    /// Build statistics.
    pub fn stats(&self) -> BuildStats {
        self.stats
    }

    /// Number of distinct normalised titles.
    pub fn len(&self) -> usize {
        self.title_off.len().saturating_sub(1)
    }

    /// True when nothing was indexed.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Heap bytes held by the index (exact, from capacities).
    pub fn heap_bytes(&self) -> usize {
        self.arena.capacity()
            + self.tok_arena.capacity()
            + 4 * (self.title_off.capacity()
                + self.entry_off.capacity()
                + self.tok_off.capacity()
                + self.post_off.capacity()
                + self.postings.capacity())
            + 4 * self.ids.capacity()
            + 2 * self.pops.capacity()
    }

    fn title(&self, i: usize) -> &str {
        &self.arena[self.title_off[i] as usize..self.title_off[i + 1] as usize]
    }

    fn entries_of(&self, i: usize) -> impl Iterator<Item = Entry> + '_ {
        let r = self.entry_off[i] as usize..self.entry_off[i + 1] as usize;
        self.ids[r.clone()]
            .iter()
            .zip(&self.pops[r])
            .map(|(&id, &p)| Entry {
                id,
                popularity: from_bf16(p),
            })
    }

    fn find_title(&self, norm: &str) -> Option<usize> {
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.title(mid).cmp(norm) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn find_token(&self, tok: &str) -> Option<usize> {
        let n = self.tok_off.len().saturating_sub(1);
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let t = &self.tok_arena[self.tok_off[mid] as usize..self.tok_off[mid + 1] as usize];
            match t.cmp(tok) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// The normalised title indexed for `id` (linear scan: diagnostics and evaluation only).
    pub fn title_of(&self, id: u32) -> Option<&str> {
        let pos = self.ids.iter().position(|&e| e == id)?;
        let t = self
            .entry_off
            .partition_point(|&o| o as usize <= pos)
            .checked_sub(1)?;
        Some(self.title(t))
    }

    /// Ids whose normalised title equals `title` exactly (most popular first).
    pub fn lookup_exact(&self, title: &str) -> Vec<Entry> {
        self.find_title(&normalize_title(title))
            .map_or_else(Vec::new, |i| self.entries_of(i).collect())
    }

    /// Candidate ids for a (raw, un-normalised) title, best first.
    ///
    /// 1. Exact: the normalised title, and its article-toggled variant
    ///    (`matrix` ↔ `the matrix`).
    /// 2. Fuzzy: titles sharing the query's two rarest indexed tokens, scored
    ///    with [`title_similarity`], kept if ≥ `0.70`.
    ///
    /// Ranking is by similarity, then popularity. At most `limit` candidates,
    /// counting each id under a title separately.
    pub fn candidates(&self, title: &str, limit: usize) -> Vec<ExportCandidate> {
        let q = normalize_title(title);
        if q.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut scored: Vec<(f32, usize, HitKind)> = Vec::new(); // (similarity, title ordinal, hit)
        let mut seen: Vec<usize> = Vec::new();
        let stripped = strip_article(&q);
        let mut variants = vec![q.clone()];
        if stripped != q {
            variants.push(stripped.to_owned());
        } else {
            variants.push(format!("the {q}"));
        }
        for v in &variants {
            if let Some(i) = self.find_title(v) {
                if !seen.contains(&i) {
                    seen.push(i);
                    scored.push((title_similarity(&q, self.title(i)), i, HitKind::Exact));
                }
            }
        }

        // Fuzzy via the rarest tokens. Stop tokens above `max_token_df` have
        // empty postings; rank them last so they never crowd out real words.
        let df = |t: usize| match self.post_off[t + 1] - self.post_off[t] {
            0 => u32::MAX,
            n => n,
        };
        let mut toks: Vec<(u32, usize)> = unique_tokens(stripped)
            .filter_map(|t| self.find_token(t).map(|id| (df(id), id)))
            .collect();
        toks.sort_unstable();
        const MAX_SCAN: usize = 20_000;
        let mut scanned = 0usize;
        for &(_, t) in toks.iter().take(2) {
            let posts = &self.postings[self.post_off[t] as usize..self.post_off[t + 1] as usize];
            for &ord in posts.iter().take(MAX_SCAN.saturating_sub(scanned)) {
                scanned += 1;
                let ord = ord as usize;
                if seen.contains(&ord) {
                    continue;
                }
                let sim = title_similarity(&q, self.title(ord));
                if sim >= 0.70 {
                    scored.push((sim, ord, HitKind::Fuzzy));
                }
            }
        }
        // `seen` only tracks exact hits; dedupe fuzzy duplicates from the two token lists.
        scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.dedup_by_key(|s| s.1);

        let mut out: Vec<ExportCandidate> = Vec::new();
        for (sim, ord, hit) in scored {
            for e in self.entries_of(ord) {
                out.push(ExportCandidate {
                    id: e.id,
                    kind: self.kind,
                    title: self.title(ord).to_owned(),
                    popularity: e.popularity,
                    similarity: sim,
                    hit,
                });
            }
        }
        // Similarity first, popularity as the tie-breaker (remakes share a title).
        out.sort_by(|a, b| {
            b.similarity
                .total_cmp(&a.similarity)
                .then(b.popularity.total_cmp(&a.popularity))
        });
        out.truncate(limit);
        out
    }
}

/// f32 → bfloat16 (round to nearest even on the dropped 16 bits).
fn to_bf16(x: f32) -> u16 {
    let b = x.to_bits();
    let round = 0x7FFF + ((b >> 16) & 1);
    (b.wrapping_add(round) >> 16) as u16
}

fn from_bf16(x: u16) -> f32 {
    f32::from_bits(u32::from(x) << 16)
}

fn unique_tokens(s: &str) -> impl Iterator<Item = &str> {
    let mut seen: Vec<&str> = Vec::new();
    s.split(' ').filter(move |t| {
        if t.is_empty() || seen.contains(t) {
            false
        } else {
            seen.push(t);
            true
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"adult":false,"id":603,"original_title":"The Matrix","popularity":48.1,"video":false}
{"adult":false,"id":1386216,"original_title":"The Matrix","popularity":0.9,"video":false}
{"adult":false,"id":604,"original_title":"The Matrix Reloaded","popularity":30.2,"video":false}
{"adult":false,"id":496243,"original_title":"기생충","popularity":39.9,"video":false}
{"adult":false,"id":194,"original_title":"Le Fabuleux Destin d'Amélie Poulain","popularity":26.2,"video":false}
{"adult":true,"id":9999,"original_title":"The Matrix XXX","popularity":5.0,"video":false}
{"adult":false,"id":8888,"original_title":"The Matrix Making Of","popularity":1.0,"video":true}
{"adult":false,"id":10681,"original_title":"WALL·E","popularity":33.5,"video":false}
{"adult":false,"id":862,"original_title":"Toy Story","popularity":49.2,"video":null}
not json at all
{"adult":false,"id":863,"original_title":"Toy Story 2","popularity":49.7,"video":false}
"#;

    fn index() -> ExportIndex {
        ExportIndex::build_from_reader(SAMPLE.as_bytes(), ExportKind::Movie, BuildOptions::default()).unwrap()
    }

    #[test]
    fn file_names() {
        assert_eq!(
            export_file_name(ExportKind::Movie, 20_744),
            "movie_ids_10_18_2026.json.gz"
        );
        assert_eq!(
            export_url(ExportKind::Tv, 20_744),
            "https://files.tmdb.org/p/exports/tv_series_ids_10_18_2026.json.gz"
        );
    }

    #[test]
    fn builds_and_filters() {
        let idx = index();
        let s = idx.stats();
        assert_eq!(s.rows_read, 11);
        assert_eq!(s.rows_skipped, 2); // adult + video
        assert_eq!(s.rows_malformed, 1);
        assert_eq!(s.distinct_titles, 7);
        assert!(idx.heap_bytes() > 0);
    }

    #[test]
    fn bf16_popularity_is_close() {
        for p in [0.0f32, 0.6, 7.92, 48.13, 1234.5] {
            let back = from_bf16(to_bf16(p));
            assert!((back - p).abs() <= p * 0.004 + 1e-6, "{p} -> {back}");
        }
    }

    #[test]
    fn exact_lookup_most_popular_first() {
        let idx = index();
        let hits = idx.lookup_exact("the matrix");
        assert_eq!(hits.iter().map(|e| e.id).collect::<Vec<_>>(), vec![603, 1386216]);
        assert_eq!(idx.lookup_exact("기생충")[0].id, 496243);
        assert_eq!(idx.lookup_exact("WALL-E")[0].id, 10681);
        assert!(idx.lookup_exact("Parasite").is_empty()); // translated title: not in the export
    }

    #[test]
    fn candidates_exact_article_and_fuzzy() {
        let idx = index();
        let c = idx.candidates("Matrix", 5);
        assert_eq!(c[0].id, 603);
        assert_eq!(c[0].hit, HitKind::Exact);
        let c = idx.candidates("Le Fabuleux Destin d'Amelie Poulain", 5);
        assert_eq!((c[0].id, c[0].hit), (194, HitKind::Exact), "{c:?}");
        let c = idx.candidates("Fabuleux Destin d'Amélie Poulain", 5);
        assert_eq!((c[0].id, c[0].hit), (194, HitKind::Fuzzy), "{c:?}");
        // Sequel numbers matter: "Toy Story 2" must beat the more-similar-looking original.
        let c = idx.candidates("Toy Story 2", 5);
        assert_eq!(c[0].id, 863);
    }
}
