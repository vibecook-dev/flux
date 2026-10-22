//! The durable job queue: flux's one queue for background work that must
//! survive a restart (matching, artwork). flux-meta's in-memory `JobQueue`
//! is the *executor* (lanes, budgets); this table is what it polls.
//!
//! ```text
//!   apply (new/changed items) ─► enqueue ─► queued ──claim (lease)──► running
//!                                              ▲                        │
//!            backoff: base·2^(attempt-1) ──────┴── Retry ◄──────────────┤
//!                                                      Done / Fail ◄────┘
//!   a lease that expires without a finish counts as a failed attempt
//! ```
//!
//! - **Idempotency:** `(kind, key)` is unique. Enqueueing a key that is
//!   queued, running, done or failed is a no-op ([`Enqueued::Duplicate`]), so
//!   a rescan never re-runs finished work. A key is content-derived (e.g. the
//!   title/year the matcher will use), so a real change gets a new key.
//! - **Supersede:** at most one queued job per `(kind, item)`. A new key for
//!   an item whose job is still queued replaces that job's key and payload;
//!   finished jobs of older keys for the item are forgotten.
//! - **Leases:** [`crate::Writer::claim_jobs`] hands out jobs with a lease; the worker finishes
//!   (or extends) it by `(id, attempt)`. A finish for a lease that expired
//!   and was re-claimed is refused ([`Finished::LeaseLost`]).
//! - **Retries:** exponential backoff with deterministic jitter, up to
//!   `max_attempts` (per job), then `failed`.
//! - **Priorities:** lower runs first ([`priority`]); FIFO within a priority,
//!   except art jobs, which run newest first (what was just added gets art
//!   first). [`crate::Writer::bump_art`] moves one work's art to the front.
//! - **Dependencies:** `after` makes a job wait until another one finished
//!   (an item's art waits for its match).

use std::time::Duration;

use flux_core::api::Id;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};

use crate::error::{CatalogError, Result};

/// Match an item against metadata providers (flux-meta).
pub const JOB_MATCH: &str = "match";
/// Find or make artwork for an item: provider art after a match, a frame
/// poster or episode thumb otherwise (flux-meta).
pub const JOB_ART: &str = "art";

/// Job priorities: lower runs first.
pub mod priority {
    /// The user is waiting (e.g. "fix match").
    pub const INTERACTIVE: i64 = 0;
    /// Items with no usable title (they look worst in the UI).
    pub const UNMATCHED: i64 = 10;
    pub const MATCH: i64 = 20;
    pub const ART: i64 = 40;
    pub const BACKGROUND: i64 = 60;
}

/// A job to enqueue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    pub kind: String,
    /// Idempotency key, unique per kind.
    pub key: String,
    pub item_id: Option<Id>,
    pub priority: i64,
    pub payload: Option<String>,
    /// `None` = [`crate::WriterConfig::job_max_attempts`].
    pub max_attempts: Option<u32>,
    /// Don't start before this job finished (done or failed).
    pub after: Option<Id>,
    /// Don't start before this time (Unix ms).
    pub not_before: Option<i64>,
}

impl JobSpec {
    pub fn new(kind: impl Into<String>, key: impl Into<String>) -> JobSpec {
        JobSpec {
            kind: kind.into(),
            key: key.into(),
            item_id: None,
            priority: priority::BACKGROUND,
            payload: None,
            max_attempts: None,
            after: None,
            not_before: None,
        }
    }
    pub fn item(mut self, item_id: Id) -> Self {
        self.item_id = Some(item_id);
        self
    }
    pub fn priority(mut self, p: i64) -> Self {
        self.priority = p;
        self
    }
    pub fn payload(mut self, p: impl Into<String>) -> Self {
        self.payload = Some(p.into());
        self
    }
}

/// What [`crate::Writer::enqueue_job`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    New(Id),
    /// The key already exists (queued, running, done or failed): nothing to do.
    Duplicate(Id),
    /// The item's queued job of this kind now carries the new key.
    Superseded(Id),
}

impl Enqueued {
    pub fn id(self) -> Id {
        match self {
            Enqueued::New(id) | Enqueued::Duplicate(id) | Enqueued::Superseded(id) => id,
        }
    }
}

/// Parameters of [`crate::Writer::claim_jobs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimJobs {
    /// Who holds the lease (diagnostics only; the lease is `(id, attempt)`).
    pub worker: String,
    /// Kinds this worker runs; empty = any.
    pub kinds: Vec<String>,
    pub limit: u32,
    pub lease: Duration,
    /// Clock override for tests; `None` = now.
    pub now_ms: Option<i64>,
}

impl ClaimJobs {
    pub fn new(worker: impl Into<String>, kinds: &[&str], limit: u32, lease: Duration) -> ClaimJobs {
        ClaimJobs {
            worker: worker.into(),
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
            limit,
            lease,
            now_ms: None,
        }
    }
}

/// A claimed job. Finish it with [`crate::Writer::finish_job`] (or extend the
/// lease with [`crate::Writer::extend_job_lease`]) before `lease_until`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLease {
    pub id: Id,
    pub kind: String,
    pub key: String,
    pub item_id: Option<Id>,
    pub payload: Option<String>,
    pub priority: i64,
    /// 1 for the first try; together with `id` it identifies the lease.
    pub attempt: u32,
    pub max_attempts: u32,
    pub lease_until: i64,
}

/// How a job attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobResult {
    Done,
    /// Transient failure: retry after the backoff (until `max_attempts`).
    Retry(String),
    /// Permanent failure: don't retry.
    Fail(String),
}

/// The job's state after [`crate::Writer::finish_job`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    Done,
    /// Queued again; runs at or after this time (Unix ms).
    RetryAt(i64),
    Failed,
    /// The lease had expired and the job was re-claimed (or finished, or
    /// deleted): this result was discarded.
    LeaseLost,
}

/// Retry policy (from [`crate::WriterConfig`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub base_ms: i64,
    pub max_ms: i64,
    pub max_attempts: u32,
}

impl Backoff {
    /// Delay before attempt `attempt + 1`: `base · 2^(attempt-1)`, capped,
    /// plus up to 12.5 % jitter derived from the job id (spreads a burst of
    /// failures without randomness in tests).
    pub fn delay_ms(&self, job_id: Id, attempt: u32) -> i64 {
        let exp = attempt.saturating_sub(1).min(30);
        let d = self.base_ms.saturating_mul(1i64 << exp).min(self.max_ms).max(0);
        let jitter = (job_id as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 61; // 0..=7
        d.saturating_add(d / 8 * jitter as i64 / 7)
    }
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Enqueue one job (dedupe by key, supersede per item). Runs on the writer.
pub(crate) fn enqueue(c: &Connection, s: &JobSpec, now: i64, default_max: u32) -> Result<Enqueued> {
    if s.kind.is_empty() || s.key.is_empty() {
        return Err(CatalogError::Invalid("job kind and key must not be empty".into()));
    }
    let existing: Option<(Id, String, i64)> = c
        .prepare_cached("SELECT id, state, priority FROM job WHERE kind = ?1 AND key = ?2")?
        .query_row(params![s.kind, s.key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    if let Some((id, state, prio)) = existing {
        if state == "queued" && s.priority < prio {
            c.prepare_cached("UPDATE job SET priority = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![id, s.priority, now])?;
        }
        return Ok(Enqueued::Duplicate(id));
    }
    let max = s.max_attempts.unwrap_or(default_max).max(1);
    let not_before = s.not_before.unwrap_or(now);
    if let Some(item) = s.item_id {
        let superseded: Option<Id> = c
            .prepare_cached(
                "UPDATE job SET key = ?3, payload = ?4, priority = MIN(priority, ?5), max_attempts = ?6,
                                after_id = ?7, not_before = ?8, attempts = 0, last_error = NULL, updated_at = ?9
                 WHERE kind = ?1 AND item_id = ?2 AND state = 'queued' RETURNING id",
            )?
            .query_row(
                params![s.kind, item, s.key, s.payload, s.priority, max, s.after, not_before, now],
                |r| r.get(0),
            )
            .optional()?;
        c.prepare_cached(
            "DELETE FROM job WHERE kind = ?1 AND item_id = ?2 AND key <> ?3 AND state IN ('done','failed')",
        )?
        .execute(params![s.kind, item, s.key])?;
        if let Some(id) = superseded {
            return Ok(Enqueued::Superseded(id));
        }
    }
    c.prepare_cached(
        "INSERT INTO job(kind, key, item_id, priority, max_attempts, not_before, after_id, payload, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
    )?
    .execute(params![
        s.kind,
        s.key,
        s.item_id,
        s.priority,
        max,
        not_before,
        s.after,
        s.payload,
        now
    ])?;
    Ok(Enqueued::New(c.last_insert_rowid()))
}

/// Fast path for the scanner's apply: the match and art jobs of a top-level
/// item created in this transaction (its keys cannot exist yet, nothing to
/// supersede). The art job waits for the match.
pub(crate) fn enqueue_for_new_work(
    c: &Connection,
    item_id: Id,
    match_key: &str,
    unmatched: bool,
    now: i64,
    max_attempts: u32,
) -> Result<()> {
    let prio = if unmatched {
        priority::UNMATCHED
    } else {
        priority::MATCH
    };
    let n = c
        .prepare_cached(
            "INSERT OR IGNORE INTO job(kind, key, item_id, priority, max_attempts, not_before, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?6)",
        )?
        .execute(params![JOB_MATCH, match_key, item_id, prio, max_attempts, now])?;
    let match_id = (n > 0).then(|| c.last_insert_rowid());
    c.prepare_cached(
        "INSERT OR IGNORE INTO job(kind, key, item_id, priority, max_attempts, not_before, after_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?6)",
    )?
    .execute(params![
        JOB_ART,
        art_key(item_id, now),
        item_id,
        priority::ART,
        max_attempts,
        now,
        match_id
    ])?;
    Ok(())
}

/// A show gained episodes: (re)arm its art job so the executor makes thumbs
/// for them. Supersedes a queued art job of the show (one pending per show);
/// waits for the show's match if one is pending.
pub(crate) fn rearm_art(c: &Connection, show_id: Id, now: i64, max_attempts: u32) -> Result<Enqueued> {
    arm_art(c, show_id, &art_key(show_id, now), now, max_attempts)
}

/// Queue an art job for every movie or show with stale images
/// ([`crate::read::stale_art`] for art `version`): the job makes them again,
/// then deletes the stale ones. Returns how many works got a job.
///
/// The key names the art version and the stale images, so a restart
/// doesn't queue the same refresh again once it ran, or failed (a file that
/// gives no usable frame keeps its old image rather than none).
pub(crate) fn refresh_stale_art(c: &Connection, version: u32, now: i64, max_attempts: u32) -> Result<u64> {
    let mut works: std::collections::BTreeMap<Id, (Id, u64)> = std::collections::BTreeMap::new();
    for s in crate::read::stale_art(c, version, None)? {
        let e = works.entry(s.work_id).or_insert((0, 0));
        e.0 = e.0.max(s.art_id);
        e.1 += 1;
    }
    let mut queued = 0;
    for (work, (newest, n)) in works {
        let key = format!("{work}:refresh:v{version}:{newest}:{n}");
        if let Enqueued::New(_) | Enqueued::Superseded(_) = arm_art(c, work, &key, now, max_attempts)? {
            queued += 1;
        }
    }
    Ok(queued)
}

/// Queue (or supersede) the art job of a movie or show under `key`; it
/// waits for the work's match if one is pending.
fn arm_art(c: &Connection, work: Id, key: &str, now: i64, max_attempts: u32) -> Result<Enqueued> {
    let pending_match: Option<Id> = c
        .prepare_cached(
            "SELECT id FROM job WHERE item_id = ?1 AND kind = ?2 AND state IN ('queued','running') LIMIT 1",
        )?
        .query_row(params![work, JOB_MATCH], |r| r.get(0))
        .optional()?;
    let mut spec = JobSpec::new(JOB_ART, key).item(work).priority(priority::ART);
    spec.after = pending_match;
    enqueue(c, &spec, now, max_attempts)
}

/// The UI asked for art an item doesn't have yet: move the art job of its
/// work (movie or show; one art job covers seasons and episode thumbs) to
/// the front of the queue, with the match job it waits for. Returns whether
/// a queued job was found (already-running or finished jobs are left alone).
pub(crate) fn bump_art(c: &Connection, item_id: Id, now: i64) -> Result<bool> {
    // The top-level work (episode → season → show).
    let mut top = item_id;
    for _ in 0..4 {
        let parent: Option<Option<Id>> = c
            .prepare_cached("SELECT parent_id FROM item WHERE id = ?1")?
            .query_row([top], |r| r.get(0))
            .optional()?;
        match parent {
            Some(Some(p)) => top = p,
            Some(None) => break,
            None => return Ok(false),
        }
    }
    let art: Option<(Id, Option<Id>)> = c
        .prepare_cached(
            "SELECT id, after_id FROM job WHERE kind = ?1 AND item_id = ?2 AND state = 'queued'
             ORDER BY id DESC LIMIT 1",
        )?
        .query_row(params![JOB_ART, top], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((art_id, after)) = art else {
        return Ok(false);
    };
    for id in std::iter::once(art_id).chain(after) {
        c.prepare_cached(
            "UPDATE job SET priority = ?2, not_before = MIN(not_before, ?3), updated_at = ?3
              WHERE id = ?1 AND state = 'queued' AND (priority > ?2 OR not_before > ?3)",
        )?
        .execute(params![id, priority::INTERACTIVE, now])?;
    }
    Ok(true)
}

/// The match job key of a top-level item: what the matcher works from, so a
/// rescan that changes nothing never re-matches, and a learned year or id does.
pub fn match_key(item_id: Id, group_key: &str, year: Option<u16>, ids: &flux_core::ProviderIds) -> String {
    let opt = |v: Option<String>| v.unwrap_or_default();
    format!(
        "{item_id}:{group_key}|{}|{}|{}|{}",
        opt(year.map(|y| y.to_string())),
        opt(ids.tmdb.map(|v| v.to_string())),
        opt(ids.tvdb.map(|v| v.to_string())),
        opt(ids.imdb.clone())
    )
}

/// The art job key of a movie or show: the item and the time its content
/// last grew. One art job covers the work, its seasons and its episodes'
/// thumbs (the executor decides provider art vs frames); new episodes re-arm
/// it with a new key.
pub fn art_key(item_id: Id, generation: i64) -> String {
    format!("{item_id}:{generation}")
}

/// Expire overdue leases (each counts as a failed attempt), then lease up to
/// `limit` ready jobs: priority, then age; dependencies must have finished.
pub(crate) fn claim(c: &Connection, req: &ClaimJobs, backoff: &Backoff) -> Result<Vec<JobLease>> {
    let now = req.now_ms.unwrap_or_else(crate::model::now_ms);
    expire_leases(c, now, backoff)?;
    // One ordered walk of `job_ready` (per kind) for each kind, merged.
    let kinds: Vec<String> = if req.kinds.is_empty() {
        let mut stmt = c.prepare_cached("SELECT kind FROM job WHERE state = 'queued' GROUP BY kind")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    } else {
        req.kinds.clone()
    };
    let limit = req.limit.min(10_000);
    let lease_until = now.saturating_add(ms(req.lease));
    let to_u32 = |v: i64| v.clamp(1, i64::from(u32::MAX)) as u32;
    let mut cands: Vec<(i64, JobLease)> = Vec::new();
    for kind in &kinds {
        // Art: newest first within a priority, so what the user just added
        // (the top of "Recently added") gets art before the back catalogue.
        // Everything else: FIFO. (`?1` is the kind in both; the art query
        // names it literally so the partial index applies.)
        let sql = if kind == JOB_ART {
            "SELECT j.id, j.kind, j.key, j.item_id, j.payload, j.priority, j.attempts, j.max_attempts, j.not_before
               FROM job j INDEXED BY job_ready_art
              WHERE j.state = 'queued' AND j.kind = 'art' AND j.kind = ?1 AND j.not_before <= ?2
                AND (j.after_id IS NULL OR NOT EXISTS
                     (SELECT 1 FROM job d WHERE d.id = j.after_id AND d.state IN ('queued','running')))
              ORDER BY j.priority, j.id DESC LIMIT ?3"
        } else {
            "SELECT j.id, j.kind, j.key, j.item_id, j.payload, j.priority, j.attempts, j.max_attempts, j.not_before
               FROM job j INDEXED BY job_ready
              WHERE j.state = 'queued' AND j.kind = ?1 AND j.not_before <= ?2
                AND (j.after_id IS NULL OR NOT EXISTS
                     (SELECT 1 FROM job d WHERE d.id = j.after_id AND d.state IN ('queued','running')))
              ORDER BY j.priority, j.not_before, j.id LIMIT ?3"
        };
        let mut stmt = c.prepare_cached(sql)?;
        let rows = stmt.query_map(params![kind, now, limit], |r| {
            Ok((
                r.get::<_, i64>(8)?,
                JobLease {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    key: r.get(2)?,
                    item_id: r.get(3)?,
                    payload: r.get(4)?,
                    priority: r.get(5)?,
                    attempt: to_u32(r.get::<_, i64>(6)? + 1),
                    max_attempts: to_u32(r.get(7)?),
                    lease_until,
                },
            ))
        })?;
        for row in rows {
            cands.push(row?);
        }
    }
    // Merge the kinds: by priority, then each kind's own order (art newest
    // first, the rest FIFO).
    cands.sort_by_key(|(not_before, j)| {
        let order = if j.kind == JOB_ART {
            (0, -j.id)
        } else {
            (*not_before, j.id)
        };
        (j.priority, order)
    });
    cands.truncate(limit as usize);
    let out: Vec<JobLease> = cands.into_iter().map(|(_, j)| j).collect();
    for j in &out {
        c.prepare_cached(
            "UPDATE job SET state = 'running', attempts = attempts + 1, lease_owner = ?2, lease_until = ?3,
                            updated_at = ?4 WHERE id = ?1",
        )?
        .execute(params![j.id, req.worker, lease_until, now])?;
    }
    Ok(out)
}

/// Running jobs whose lease ran out: the worker died or hung. The attempt
/// counts; retry after the backoff, or fail at `max_attempts`.
pub(crate) fn expire_leases(c: &Connection, now: i64, backoff: &Backoff) -> Result<u64> {
    let expired: Vec<(Id, i64, i64)> = {
        let mut stmt = c.prepare_cached(
            "SELECT id, attempts, max_attempts FROM job WHERE state = 'running' AND lease_until < ?1",
        )?;
        let rows = stmt.query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for &(id, attempts, max) in &expired {
        settle_failure(c, id, attempts, max, "lease expired", true, now, backoff)?;
    }
    Ok(expired.len() as u64)
}

#[allow(clippy::too_many_arguments)]
fn settle_failure(
    c: &Connection,
    id: Id,
    attempts: i64,
    max: i64,
    error: &str,
    retryable: bool,
    now: i64,
    backoff: &Backoff,
) -> Result<Finished> {
    if retryable && attempts < max {
        let at = now.saturating_add(backoff.delay_ms(id, attempts.clamp(0, i64::from(u32::MAX)) as u32));
        c.prepare_cached(
            "UPDATE job SET state = 'queued', not_before = ?2, last_error = ?3, lease_owner = NULL,
                            lease_until = NULL, updated_at = ?4 WHERE id = ?1",
        )?
        .execute(params![id, at, error, now])?;
        Ok(Finished::RetryAt(at))
    } else {
        c.prepare_cached(
            "UPDATE job SET state = 'failed', last_error = ?2, lease_owner = NULL, lease_until = NULL,
                            updated_at = ?3, finished_at = ?3 WHERE id = ?1",
        )?
        .execute(params![id, error, now])?;
        Ok(Finished::Failed)
    }
}

/// Finish a leased attempt. Refused (no change) when `(id, attempt)` no
/// longer holds the lease.
pub(crate) fn finish(
    c: &Connection,
    id: Id,
    attempt: u32,
    result: &JobResult,
    now: i64,
    backoff: &Backoff,
) -> Result<Finished> {
    let row: Option<(i64, i64)> = c
        .prepare_cached(
            "SELECT attempts, max_attempts FROM job WHERE id = ?1 AND state = 'running' AND attempts = ?2",
        )?
        .query_row(params![id, attempt], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((attempts, max)) = row else {
        return Ok(Finished::LeaseLost);
    };
    match result {
        JobResult::Done => {
            c.prepare_cached(
                "UPDATE job SET state = 'done', last_error = NULL, lease_owner = NULL, lease_until = NULL,
                                updated_at = ?2, finished_at = ?2 WHERE id = ?1",
            )?
            .execute(params![id, now])?;
            Ok(Finished::Done)
        }
        JobResult::Retry(e) => settle_failure(c, id, attempts, max, e, true, now, backoff),
        JobResult::Fail(e) => settle_failure(c, id, attempts, max, e, false, now, backoff),
    }
}

/// Extend a held lease (long jobs heartbeat). `false` if the lease is gone.
pub(crate) fn extend(c: &Connection, id: Id, attempt: u32, lease: Duration, now: i64) -> Result<bool> {
    Ok(c
        .prepare_cached(
            "UPDATE job SET lease_until = ?3, updated_at = ?4 WHERE id = ?1 AND attempts = ?2 AND state = 'running'",
        )?
        .execute(params![id, attempt, now.saturating_add(ms(lease)), now])?
        > 0)
}

/// Queue failed jobs again (admin "retry"), with fresh attempts.
pub(crate) fn requeue_failed(c: &Connection, kind: Option<&str>, now: i64) -> Result<u64> {
    Ok(c.prepare_cached(
        "UPDATE job SET state = 'queued', attempts = 0, not_before = ?2, finished_at = NULL, updated_at = ?2
             WHERE state = 'failed' AND (?1 IS NULL OR kind = ?1)",
    )?
    .execute(params![kind, now])? as u64)
}

// ---- reads ---------------------------------------------------------------------

/// Jobs per `(kind, state)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCount {
    pub kind: String,
    pub state: String,
    pub count: u64,
}

/// Queue depth by kind and state (admin page, metrics).
pub fn job_counts(c: &Connection) -> Result<Vec<JobCount>> {
    let mut stmt =
        c.prepare_cached("SELECT kind, state, count(*) FROM job GROUP BY kind, state ORDER BY kind, state")?;
    let rows = stmt.query_map([], |r| {
        Ok(JobCount {
            kind: r.get(0)?,
            state: r.get(1)?,
            count: r.get::<_, i64>(2)?.max(0) as u64,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// When the executor should poll next for `kinds` (empty = any): the
/// earliest `not_before` of a queued job or `lease_until` of a running one.
/// `None` = nothing pending. A value in the past means "now".
pub fn next_job_due(c: &Connection, kinds: &[&str]) -> Result<Option<i64>> {
    let kinds_sql = if kinds.is_empty() {
        String::new()
    } else {
        format!(" AND kind IN ({})", vec!["?"; kinds.len()].join(","))
    };
    let sql = format!(
        "SELECT min(t) FROM (
            SELECT min(not_before) AS t FROM job WHERE state = 'queued'{kinds_sql}
            UNION ALL SELECT min(lease_until) FROM job WHERE state = 'running'{kinds_sql})"
    );
    let args: Vec<&str> = kinds.iter().chain(kinds.iter()).copied().collect();
    Ok(c.prepare_cached(&sql)?
        .query_row(params_from_iter(args.iter()), |r| r.get(0))?)
}

/// A job row, for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    pub id: Id,
    pub kind: String,
    pub key: String,
    pub item_id: Option<Id>,
    pub state: String,
    pub priority: i64,
    pub attempts: u32,
    pub max_attempts: u32,
    pub not_before: i64,
    pub lease_until: Option<i64>,
    pub last_error: Option<String>,
}

/// One job by id.
pub fn job(c: &Connection, id: Id) -> Result<Option<JobInfo>> {
    Ok(c
        .prepare_cached(
            "SELECT id, kind, key, item_id, state, priority, attempts, max_attempts, not_before, lease_until, last_error
             FROM job WHERE id = ?1",
        )?
        .query_row([id], |r| {
            Ok(JobInfo {
                id: r.get(0)?,
                kind: r.get(1)?,
                key: r.get(2)?,
                item_id: r.get(3)?,
                state: r.get(4)?,
                priority: r.get(5)?,
                attempts: r.get::<_, i64>(6)?.clamp(0, i64::from(u32::MAX)) as u32,
                max_attempts: r.get::<_, i64>(7)?.clamp(0, i64::from(u32::MAX)) as u32,
                not_before: r.get(8)?,
                lease_until: r.get(9)?,
                last_error: r.get(10)?,
            })
        })
        .optional()?)
}

/// Jobs of an item (any state), newest first.
pub fn item_jobs(c: &Connection, item_id: Id) -> Result<Vec<JobInfo>> {
    let ids: Vec<Id> = {
        let mut stmt = c.prepare_cached("SELECT id FROM job WHERE item_id = ?1 ORDER BY id DESC")?;
        let rows = stmt.query_map([item_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(j) = job(c, id)? {
            out.push(j);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_caps_and_jitters_within_an_eighth() {
        let b = Backoff {
            base_ms: 1_000,
            max_ms: 60_000,
            max_attempts: 5,
        };
        for id in 1..50 {
            let d1 = b.delay_ms(id, 1);
            let d3 = b.delay_ms(id, 3);
            assert!((1_000..=1_125).contains(&d1), "{d1}");
            assert!((4_000..=4_500).contains(&d3), "{d3}");
            assert!(b.delay_ms(id, 30) <= 60_000 + 60_000 / 8);
        }
    }
}
