//! Background jobs: metadata matching and artwork (flux-meta), executed from
//! the catalog's durable job queue.
//!
//! Out of the way by construction:
//! - one title at a time; frame posters with 2 parallel ffmpeg decodes;
//! - ffmpeg/ffprobe run through a `nice -n 19` wrapper; the export index is
//!   built on a background-priority thread;
//! - paused while anyone streamed in the last 30 s or a library is scanning.
//!
//! The queue (`flux_catalog::jobs`): the scanner's apply enqueues a `match`
//! and an `art` job for every new movie or show (art waits for the match)
//! and re-arms a show's art job when it gains episodes. This loop leases
//! them (`claim_jobs`), finishes each attempt (`finish_job`: done, retry
//! with backoff, or fail) and sleeps until `next_job_due`.
//! - **match:** candidates come from TMDB when `TMDB_READ_TOKEN` /
//!   `TMDB_API_KEY` is set, else from the offline TMDB ID exports if present.
//!   With neither, match jobs fail at once (so art can run) and are queued
//!   again at the next start that has a source. An accepted match whose
//!   provider ids another work of the library already has merges the two
//!   (`merge_items`).
//! - **art:** one job per movie or show: a frame poster + backdrop when the
//!   item has none, then (shows) a thumbnail for every episode without one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flux_catalog::jobs::{art_key, next_job_due, priority};
use flux_catalog::{
    read, ArtKind, ClaimJobs, Finished, ItemMetadata, JobLease, JobResult, JobSpec, NewArt, JOB_ART,
    JOB_MATCH,
};
use flux_core::api::{Event, Id, ItemKind, ScanState};
use flux_core::ProbeResult;
use flux_meta::art::{self, ArtImage, FramePosterOptions, MediaFacts, Tools};
use flux_meta::export_index::{BuildOptions, ExportIndex, ExportKind};
use flux_meta::matcher::{Candidate, Decision, MatchQuery, MatchResult, Matcher, MediaKind};
use flux_meta::tmdb::{self, MovieSearch, TmdbClient};
use tokio::sync::oneshot;

use crate::state::AppState;

/// Streaming within this long pauses jobs.
const PLAYBACK_QUIET_MS: i64 = 30_000;
/// Idle poll when there is nothing to do.
const IDLE_POLL: Duration = Duration::from_secs(30);
/// The export index (≈60 MB) is dropped after this long without match work.
const EXPORTS_TTL: Duration = Duration::from_secs(600);
/// Lease owner name in the job table (diagnostics only).
const WORKER: &str = "flux-server";
/// Match jobs leased at once (run one after another).
const MATCH_BATCH: u32 = 4;
/// Lease of a match batch; each item takes ≤ a few TMDB round trips.
const MATCH_LEASE: Duration = Duration::from_secs(300);
/// Lease of an art job, extended after every image.
const ART_LEASE: Duration = Duration::from_secs(120);
/// Error recorded on match jobs failed for want of a metadata source.
const NO_SOURCE: &str = "no metadata source (no TMDB key, no offline exports)";

/// Starts the job loop (returns at once).
pub fn spawn(st: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(st))
}

struct Exports {
    movie: Option<ExportIndex>,
    tv: Option<ExportIndex>,
}

struct Ctx {
    poster_opts: FramePosterOptions,
    tmdb: Option<TmdbClient>,
    image_base: Option<String>,
    exports: Option<Arc<Exports>>,
    exports_used: Instant,
}

impl Ctx {
    fn new(st: &AppState) -> Ctx {
        let tools = Tools {
            ffmpeg: low_priority_tool(&st.cfg.data_dir, &st.cfg.ffmpeg, "ffmpeg"),
            ffprobe: low_priority_tool(&st.cfg.data_dir, &st.cfg.ffprobe, "ffprobe"),
            ..Tools::default()
        };
        let poster_opts = FramePosterOptions {
            tools,
            concurrency: 2,
            ..FramePosterOptions::default()
        };
        let tmdb = TmdbClient::from_env().ok();
        if tmdb.is_some() {
            tracing::info!("TMDB credentials found: online matching on");
        }
        Ctx {
            poster_opts,
            tmdb,
            image_base: None,
            exports: None,
            exports_used: Instant::now(),
        }
    }

    fn export_files(dir: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
        crate::metadata::export_files(dir)
    }

    fn can_match(&self, st: &AppState) -> bool {
        if self.tmdb.is_some() || self.exports.is_some() {
            return true;
        }
        let (m, t) = Self::export_files(&st.cfg.tmdb_exports);
        m.is_some() || t.is_some()
    }

    async fn exports(&mut self, st: &AppState) -> Option<Arc<Exports>> {
        self.exports_used = Instant::now();
        if let Some(e) = &self.exports {
            return Some(e.clone());
        }
        let (movie, tv) = Self::export_files(&st.cfg.tmdb_exports);
        if movie.is_none() && tv.is_none() {
            return None;
        }
        let t0 = Instant::now();
        let built = run_low_priority(move || {
            let build = |p: Option<PathBuf>, k: ExportKind| {
                p.and_then(
                    |p| match ExportIndex::build_from_gz(&p, k, BuildOptions::default()) {
                        Ok(i) => Some(i),
                        Err(e) => {
                            tracing::warn!(path = %p.display(), error = %e, "cannot read TMDB export");
                            None
                        }
                    },
                )
            };
            Exports {
                movie: build(movie, ExportKind::Movie),
                tv: build(tv, ExportKind::Tv),
            }
        })
        .await
        .ok()?;
        tracing::info!(
            ms = t0.elapsed().as_millis() as u64,
            "offline TMDB export index built"
        );
        let e = Arc::new(built);
        self.exports = Some(e.clone());
        Some(e)
    }

    fn drop_idle_exports(&mut self) {
        if self.exports.is_some() && self.exports_used.elapsed() > EXPORTS_TTL {
            tracing::debug!("dropping the idle TMDB export index");
            self.exports = None;
        }
    }
}

/// A shell wrapper that runs `tool` at `nice -n 19`, so frame decodes yield
/// to playback (child processes inherit nothing from tokio's threads).
fn low_priority_tool(data_dir: &Path, tool: &Path, name: &str) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let nice = if Path::new("/usr/bin/nice").exists() {
            "/usr/bin/nice"
        } else {
            "nice"
        };
        let quoted = tool.to_string_lossy().replace('\'', "'\\''");
        let script = format!("#!/bin/sh\nexec {nice} -n 19 '{quoted}' \"$@\"\n");
        let dir = data_dir.join("bin");
        let path = dir.join(format!("{name}-low"));
        let fresh = std::fs::read_to_string(&path).is_ok_and(|s| s == script);
        if fresh {
            return path;
        }
        let ok = std::fs::create_dir_all(&dir).is_ok()
            && std::fs::write(&path, &script).is_ok()
            && std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).is_ok();
        if ok {
            return path;
        }
    }
    let _ = (data_dir, name);
    tool.to_path_buf()
}

/// Runs CPU-heavy job work on its own background-priority thread (not
/// tokio's blocking pool, whose threads also serve segment reads).
fn run_low_priority<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> oneshot::Receiver<T> {
    let (tx, rx) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("flux-jobs-bg".into())
        .spawn(move || {
            lower_this_thread();
            let _ = tx.send(f());
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start a job thread");
    }
    rx
}

#[allow(unsafe_code)]
fn lower_this_thread() {
    // SAFETY: setpriority only changes the scheduling class of the calling thread.
    #[cfg(target_os = "macos")]
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG);
    }
    // On Linux, PRIO_PROCESS with who = 0 applies to the calling thread.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }
}

async fn run(st: Arc<AppState>) {
    if !sleep_or_shutdown(&st, st.cfg.jobs_delay).await {
        return;
    }
    let mut ctx = Ctx::new(&st);
    if ctx.can_match(&st) {
        // Match jobs failed for want of a source (or out of attempts) get
        // another go now that there is one.
        match st
            .catalog
            .writer()
            .requeue_failed_jobs(Some(JOB_MATCH.into()))
            .await
        {
            Ok(n) if n > 0 => tracing::info!(jobs = n, "match jobs queued again"),
            Ok(_) => {}
            Err(e) => tracing::debug!(error = %e, "requeue_failed_jobs"),
        }
    }
    let mut events = st.events.subscribe();
    // Polls in a row that found nothing to run although a job was due (it
    // waits on another job, e.g. art on a match in backoff): back off.
    let mut idle_polls = 0u32;
    loop {
        if st.shutdown.is_cancelled() {
            break;
        }
        if busy(&st).await {
            if !sleep_or_shutdown(&st, Duration::from_secs(5)).await {
                break;
            }
            continue;
        }
        let mut did = match_batch(&st, &mut ctx).await;
        if did == 0 && !busy(&st).await {
            did += art_batch(&st, &ctx).await;
        }
        ctx.drop_idle_exports();
        if did > 0 {
            idle_polls = 0;
            tokio::task::yield_now().await;
            continue;
        }
        // Idle: wake when the next job is due, on new items, or on shutdown.
        let due = st
            .catalog
            .read(|c| next_job_due(c, &[JOB_MATCH, JOB_ART]))
            .await
            .ok()
            .flatten();
        let nap = match due {
            Some(t) => {
                let ms = t
                    .saturating_sub(flux_catalog::now_ms())
                    .clamp(0, IDLE_POLL.as_millis() as i64);
                // Due already, yet nothing ran: 0.2 s, 0.4 s, … up to the idle poll.
                idle_polls = if ms == 0 { idle_polls + 1 } else { 0 };
                let floor = Duration::from_millis(200 << idle_polls.saturating_sub(1).min(7));
                Duration::from_millis(ms as u64).max(floor).min(IDLE_POLL)
            }
            None => IDLE_POLL,
        };
        let wait = async {
            loop {
                match events.recv().await {
                    Ok(Event::ItemsChanged { added, .. }) if !added.is_empty() => break,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        std::future::pending::<()>().await;
                    }
                    _ => {}
                }
            }
        };
        tokio::select! {
            _ = wait => {}
            _ = st.activity.jobs_woken() => {}
            _ = tokio::time::sleep(nap) => {}
            _ = st.shutdown.cancelled() => break,
        }
    }
}

/// `false` on shutdown.
async fn sleep_or_shutdown(st: &AppState, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => true,
        _ = st.shutdown.cancelled() => false,
    }
}

/// Someone is streaming, or a library is scanning.
async fn busy(st: &AppState) -> bool {
    if st.activity.media_idle_ms() < PLAYBACK_QUIET_MS {
        return true;
    }
    let libs = st.catalog.read(read::list_libraries).await.unwrap_or_default();
    libs.iter().any(|l| {
        st.scanner
            .status(l.id)
            .is_some_and(|s| s.state == ScanState::Scanning)
    })
}

// ---- matching ---------------------------------------------------------------

async fn claim(st: &AppState, kind: &str, limit: u32, lease: Duration) -> Vec<JobLease> {
    match st
        .catalog
        .writer()
        .claim_jobs(ClaimJobs::new(WORKER, &[kind], limit, lease))
        .await
    {
        Ok(j) => j,
        Err(e) => {
            tracing::debug!(kind, error = %e, "claim_jobs failed");
            Vec::new()
        }
    }
}

async fn finish(st: &AppState, job: &JobLease, result: JobResult) {
    let reason = match &result {
        JobResult::Fail(e) | JobResult::Retry(e) => Some(e.clone()),
        JobResult::Done => None,
    };
    match st.catalog.writer().finish_job(job.id, job.attempt, result).await {
        Ok(Finished::LeaseLost) => {
            tracing::debug!(job = job.id, kind = %job.kind, "lease lost; result discarded")
        }
        Ok(Finished::Failed) => {
            tracing::debug!(job = job.id, kind = %job.kind, reason = reason.as_deref().unwrap_or("?"), "job failed")
        }
        Ok(_) => {}
        Err(e) => tracing::debug!(job = job.id, error = %e, "finish_job failed"),
    }
}

async fn match_batch(st: &AppState, ctx: &mut Ctx) -> usize {
    if !ctx.can_match(st) {
        // Nothing can match: fail the jobs so the art jobs waiting on them
        // run; they are queued again at the next start with a source.
        let jobs = claim(st, JOB_MATCH, 256, MATCH_LEASE).await;
        for job in &jobs {
            finish(st, job, JobResult::Fail(NO_SOURCE.into())).await;
        }
        return jobs.len();
    }
    let jobs = claim(st, JOB_MATCH, MATCH_BATCH, MATCH_LEASE).await;
    for job in &jobs {
        let result = match job.item_id {
            Some(id) => match match_item(st, ctx, id).await {
                Ok(()) => JobResult::Done,
                Err(e) => {
                    tracing::debug!(item = id, error = %e, "match failed");
                    JobResult::Retry(e)
                }
            },
            None => JobResult::Done,
        };
        finish(st, job, result).await;
    }
    jobs.len()
}

/// Details fetched from TMDB for the accepted candidate (online only).
struct Fetched {
    title: Option<String>,
    overview: Option<String>,
    genres: Option<Vec<String>>,
    poster: Option<String>,
    backdrop: Option<String>,
}

async fn match_item(st: &AppState, ctx: &mut Ctx, item_id: Id) -> Result<(), String> {
    let detail = st
        .catalog
        .read(move |c| read::item_detail(c, item_id, None))
        .await
        .map_err(|e| e.to_string())?;
    let Some(detail) = detail else { return Ok(()) };
    let kind = match detail.summary.kind {
        ItemKind::Movie => MediaKind::Movie,
        ItemKind::Show => MediaKind::Tv,
        _ => return Ok(()),
    };
    let probe: Option<ProbeResult> = detail.assets.iter().find_map(|a| a.probe.clone());
    let mut q = MatchQuery {
        kind: Some(kind),
        titles: vec![detail.summary.title.clone()],
        year: detail.summary.year,
        ..MatchQuery::default()
    };
    if kind == MediaKind::Movie {
        q.duration_min = detail
            .runtime_ms
            .or_else(|| probe.as_ref().and_then(|p| p.duration_ms))
            .map(|ms| ms as f64 / 60_000.0);
    }
    if let Some(p) = &probe {
        q.audio_languages = MatchQuery::from_guess(&flux_core::NameGuess::unknown(), Some(p)).audio_languages;
    }

    let mut details: Vec<(u64, Fetched)> = Vec::new();
    let candidates: Vec<Candidate> = if let Some(tmdb) = ctx.tmdb.clone() {
        online_candidates(&tmdb, kind, &q, &mut details).await?
    } else if let Some(ex) = ctx.exports(st).await {
        let index = match kind {
            MediaKind::Movie => ex.movie.as_ref(),
            MediaKind::Tv => ex.tv.as_ref(),
        };
        index
            .map(|i| i.candidates(&detail.summary.title, 10))
            .unwrap_or_default()
            .iter()
            .map(Candidate::from_export)
            .collect()
    } else {
        return Err("no metadata source".into());
    };
    let result = Matcher::default().match_candidates(&q, &candidates);
    apply_match(st, ctx, &detail, result, details).await
}

async fn online_candidates(
    tmdb: &TmdbClient,
    kind: MediaKind,
    q: &MatchQuery,
    details: &mut Vec<(u64, Fetched)>,
) -> Result<Vec<Candidate>, String> {
    let title = q.titles.first().cloned().unwrap_or_default();
    let mut out = Vec::new();
    match kind {
        MediaKind::Movie => {
            let found = tmdb
                .search_movie(&MovieSearch {
                    query: title,
                    year: q.year,
                    ..MovieSearch::default()
                })
                .await
                .map_err(|e| e.to_string())?;
            for (i, r) in found.results.iter().take(5).enumerate() {
                if i < 3 {
                    if let Ok(d) = tmdb.movie_details(r.id).await {
                        out.push(Candidate::from_tmdb_movie_details(&d));
                        details.push((
                            d.id,
                            Fetched {
                                title: Some(d.title.as_str().to_string()).filter(|s| !s.is_empty()),
                                overview: Some(d.overview.as_str().to_string()).filter(|s| !s.is_empty()),
                                genres: Some(d.genres.iter().map(|g| g.name.as_str().to_string()).collect()),
                                poster: d.poster_path.clone(),
                                backdrop: d.backdrop_path.clone(),
                            },
                        ));
                        continue;
                    }
                }
                out.push(Candidate::from_tmdb_movie(r));
            }
        }
        MediaKind::Tv => {
            let found = tmdb
                .search_tv(&title, q.year, 1)
                .await
                .map_err(|e| e.to_string())?;
            for (i, r) in found.results.iter().take(5).enumerate() {
                if i < 3 {
                    if let Ok(d) = tmdb.tv_details(r.id).await {
                        out.push(Candidate::from_tmdb_tv_details(&d));
                        details.push((
                            d.id,
                            Fetched {
                                title: Some(d.name.as_str().to_string()).filter(|s| !s.is_empty()),
                                overview: Some(d.overview.as_str().to_string()).filter(|s| !s.is_empty()),
                                genres: Some(d.genres.iter().map(|g| g.name.as_str().to_string()).collect()),
                                poster: d.poster_path.clone(),
                                backdrop: d.backdrop_path.clone(),
                            },
                        ));
                        continue;
                    }
                }
                out.push(Candidate::from_tmdb_tv(r));
            }
        }
    }
    Ok(out)
}

async fn apply_match(
    st: &AppState,
    ctx: &mut Ctx,
    detail: &flux_core::api::ItemDetail,
    r: MatchResult,
    mut details: Vec<(u64, Fetched)>,
) -> Result<(), String> {
    let item_id = detail.summary.id;
    // The decision, then the best candidate and why it scored as it did.
    let mut evidence = r.evidence.clone();
    if let Some(best) = r.best() {
        let id = best.ids.tmdb.map_or_else(String::new, |i| format!("TMDB {i} "));
        let year = best.year.map_or_else(String::new, |y| format!(" ({y})"));
        evidence.push(format!(
            "best candidate: {id}\"{}\"{year}, score {:.2}",
            best.title, best.score
        ));
        evidence.extend(best.evidence.iter().cloned());
    }
    let mut m = ItemMetadata {
        match_confidence: Some(r.confidence),
        match_evidence: Some(evidence),
        ..ItemMetadata::default()
    };
    let mut art: Vec<(ArtKind, String)> = Vec::new();
    if r.decision == Decision::Accept {
        m.matched = true;
        m.tmdb_id = r.ids.tmdb;
        m.imdb_id = r.ids.imdb.clone();
        m.tvdb_id = r.ids.tvdb;
        let fetched = r
            .ids
            .tmdb
            .and_then(|id| details.iter().position(|(d, _)| *d == id))
            .map(|i| details.swap_remove(i).1);
        if let (Some(f), Some(tmdb)) = (fetched, ctx.tmdb.clone()) {
            m.title = f.title;
            m.overview = f.overview;
            m.genres = f.genres;
            if ctx.image_base.is_none() {
                ctx.image_base = tmdb.configuration().await.ok().map(|c| c.images.secure_base_url);
            }
            if let Some(base) = &ctx.image_base {
                if let Some(p) = f.poster {
                    art.push((ArtKind::Poster, format!("{base}w780{p}")));
                }
                if let Some(p) = f.backdrop {
                    art.push((ArtKind::Backdrop, format!("{base}w1280{p}")));
                }
            }
        }
    }
    st.catalog
        .writer()
        .update_item_metadata(item_id, m)
        .await
        .map_err(|e| e.to_string())?;
    for (kind, url) in art {
        let _ = st
            .catalog
            .writer()
            .add_art(
                item_id,
                NewArt {
                    kind,
                    source: flux_core::api::ArtSource::Provider,
                    path: None,
                    url: Some(url),
                    width: None,
                    height: None,
                    blurhash: None,
                    attribution: Some(tmdb::attribution::PROVIDER_NAME.to_string()),
                    generator: None,
                },
            )
            .await;
    }
    if let Some(lib) = library_of(st, item_id).await {
        st.events.send(Event::ItemsChanged {
            library_id: lib,
            added: Vec::new(),
            updated: vec![item_id],
            removed: Vec::new(),
        });
    }
    if r.decision == Decision::Accept {
        merge_same_work(st, item_id).await?;
    }
    Ok(())
}

/// Two works of a library that now carry the same provider id are one
/// (`The Wire/` and `The Wire (2002)/`): merge them into the oldest.
async fn merge_same_work(st: &AppState, item_id: Id) -> Result<(), String> {
    let others = st
        .catalog
        .read(move |c| flux_catalog::merge::works_sharing_ids(c, item_id))
        .await
        .map_err(|e| e.to_string())?;
    let Some(into) = others.iter().copied().chain([item_id]).min() else {
        return Ok(());
    };
    for from in others.into_iter().chain([item_id]).filter(|&id| id != into) {
        let outcome = st
            .catalog
            .writer()
            .merge_items(from, into)
            .await
            .map_err(|e| format!("merging {from} into {into}: {e}"))?;
        tracing::info!(from, into, "merged two works with the same provider ids");
        if let Some(ev) = outcome.event() {
            st.events.send(ev);
        }
    }
    Ok(())
}

// ---- artwork ----------------------------------------------------------------

/// Runs one art job (one movie or show). Returns how many jobs ran.
async fn art_batch(st: &AppState, ctx: &Ctx) -> usize {
    let jobs = claim(st, JOB_ART, 1, ART_LEASE).await;
    for job in &jobs {
        let result = match job.item_id {
            Some(item) => art_job(st, ctx, job, item).await,
            None => JobResult::Done,
        };
        finish(st, job, result).await;
    }
    jobs.len()
}

async fn art_job(st: &AppState, ctx: &Ctx, job: &JobLease, item: Id) -> JobResult {
    let found = st.catalog.read(move |c| read::item_summary(c, item, None)).await;
    let summary = match found {
        Ok(Some(s)) => s,
        Ok(None) => return JobResult::Done, // deleted since
        Err(e) => return JobResult::Retry(e.to_string()),
    };
    let Some(library_id) = library_of(st, item).await else {
        return JobResult::Done;
    };
    let t0 = Instant::now();
    let mut made = 0usize;
    let episodes = if summary.kind == ItemKind::Show {
        match st
            .catalog
            .read(move |c| read::show_episodes_art(c, item, ArtKind::Thumb))
            .await
        {
            Ok(e) => e,
            Err(e) => return JobResult::Retry(e.to_string()),
        }
    } else {
        Vec::new()
    };

    // Poster + backdrop for the movie or show, unless it has both.
    let has = |kind: ArtKind| async move {
        st.catalog
            .read(move |c| read::best_art(c, item, kind))
            .await
            .ok()
            .flatten()
            .is_some()
    };
    let (poster, backdrop) = (has(ArtKind::Poster).await, has(ArtKind::Backdrop).await);
    if !(poster && backdrop) {
        let source = match summary.kind {
            ItemKind::Movie => Some(item),
            ItemKind::Show => episodes.first().map(|(e, _)| *e),
            _ => None,
        };
        let Some(source) = source else {
            return JobResult::Done; // a show without available episodes yet
        };
        match frame_art(st, ctx, item, source, &summary.title, summary.year).await {
            Ok(images) => {
                for (kind, img) in images {
                    let wanted = match kind {
                        ArtKind::Poster => !poster,
                        _ => !backdrop,
                    };
                    if wanted {
                        if let Err(e) = store_art(st, item, kind, &img).await {
                            return JobResult::Retry(e);
                        }
                        made += 1;
                    }
                }
            }
            // No available file (a sleeping drive): try again later.
            Err(ArtError::Unavailable(e)) => return JobResult::Retry(e),
            Err(ArtError::Failed(e)) => {
                tracing::info!(item, title = %summary.title, error = %e, "no frame art for this title");
                if summary.kind == ItemKind::Movie {
                    return JobResult::Fail(e);
                }
            }
        }
    }

    // Episode thumbnails (shows). Playback or a scan pauses the job: it
    // hands the rest to a fresh art job and finishes.
    for (episode, has_thumb) in episodes {
        if has_thumb {
            continue;
        }
        if st.shutdown.is_cancelled() || busy(st).await {
            let rest = JobSpec::new(JOB_ART, art_key(item, flux_catalog::now_ms()))
                .item(item)
                .priority(priority::ART);
            if let Err(e) = st.catalog.writer().enqueue_job(rest).await {
                return JobResult::Retry(e.to_string());
            }
            break;
        }
        match episode_thumb(st, ctx, episode).await {
            Ok(img) => match store_art(st, episode, ArtKind::Thumb, &img).await {
                Ok(()) => made += 1,
                Err(e) => return JobResult::Retry(e),
            },
            Err(e) => tracing::debug!(episode, error = ?e, "no thumbnail for this episode"),
        }
        let held = st
            .catalog
            .writer()
            .extend_job_lease(job.id, job.attempt, ART_LEASE)
            .await
            .unwrap_or(false);
        if !held {
            return JobResult::Retry("lease lost".into());
        }
    }
    if made > 0 {
        tracing::info!(item, title = %summary.title, images = made, ms = t0.elapsed().as_millis() as u64, "frame art ready");
        st.events.send(Event::ItemsChanged {
            library_id,
            added: Vec::new(),
            updated: vec![item],
            removed: Vec::new(),
        });
    }
    JobResult::Done
}

#[derive(Debug)]
enum ArtError {
    /// The item has no reachable file right now.
    Unavailable(String),
    Failed(String),
}

/// The first available file of an item, and its stored probe.
async fn available_file(st: &AppState, item: Id) -> Result<(PathBuf, Option<MediaFacts>), ArtError> {
    let assets = st
        .catalog
        .read(move |c| read::item_assets(c, item))
        .await
        .map_err(|e| ArtError::Unavailable(e.to_string()))?;
    let asset = assets
        .iter()
        .find(|a| a.available)
        .ok_or_else(|| ArtError::Unavailable("no available file".into()))?;
    let facts = asset.probe.as_ref().and_then(MediaFacts::from_probe);
    Ok((PathBuf::from(&asset.path), facts))
}

/// A frame poster and backdrop of `item`, from the file of `source_item`
/// (the item itself, or a show's first episode).
async fn frame_art(
    st: &AppState,
    ctx: &Ctx,
    item: Id,
    source_item: Id,
    title: &str,
    year: Option<u16>,
) -> Result<Vec<(ArtKind, ArtImage)>, ArtError> {
    let (path, facts) = available_file(st, source_item).await?;
    let fp = art::frame_posters(&path, title, year, facts, &ctx.poster_opts)
        .await
        .map_err(|e| ArtError::Failed(format!("item {item}: {e}")))?;
    Ok(vec![
        (ArtKind::Poster, fp.poster),
        (ArtKind::Backdrop, fp.backdrop),
    ])
}

async fn episode_thumb(st: &AppState, ctx: &Ctx, episode: Id) -> Result<ArtImage, ArtError> {
    let (path, facts) = available_file(st, episode).await?;
    let (thumb, _) = art::episode_thumbnail(&path, facts, &ctx.poster_opts)
        .await
        .map_err(|e| ArtError::Failed(e.to_string()))?;
    Ok(thumb)
}

/// Writes the JPEG under `<data>/art/` and adds the art row.
async fn store_art(st: &AppState, item: Id, kind: ArtKind, img: &ArtImage) -> Result<(), String> {
    let file = st.art.dir.join(format!(
        "{item}-{}-{:08x}.jpg",
        kind.as_str(),
        xxhash_rust::xxh3::xxh3_64(&img.bytes) as u32
    ));
    tokio::fs::write(&file, &img.bytes)
        .await
        .map_err(|e| format!("writing {}: {e}", file.display()))?;
    st.catalog
        .writer()
        .add_art(
            item,
            NewArt {
                kind,
                source: img.source,
                path: Some(file.to_string_lossy().into_owned()),
                url: None,
                width: Some(img.width),
                height: Some(img.height),
                blurhash: Some(img.blurhash.clone()),
                attribution: None,
                generator: img.generator.clone(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn library_of(st: &AppState, item_id: Id) -> Option<Id> {
    st.catalog
        .read(move |c| read::item_library(c, item_id))
        .await
        .ok()
        .flatten()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn low_priority_wrapper_execs_the_tool_niced() {
        let dir = tempfile::tempdir().unwrap();
        let w = low_priority_tool(dir.path(), Path::new("/bin/echo"), "echo");
        let out = std::process::Command::new(&w).arg("hi").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
        let script = std::fs::read_to_string(&w).unwrap();
        assert!(script.contains("-n 19 '/bin/echo'"), "{script}");
    }
}
