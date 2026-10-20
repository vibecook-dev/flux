//! The durable job queue: enqueue on apply, dedupe by idempotency key,
//! supersede per item, priorities, leases that expire, retries with backoff.

mod common;

use std::time::Duration;

use common::*;
use flux_catalog::jobs::{self, priority};
use flux_catalog::{
    now_ms, Catalog, CatalogConfig, ClaimJobs, Enqueued, Finished, JobResult, JobSpec, WriterConfig, JOB_ART,
    JOB_MATCH,
};
use flux_core::api::CreateLibrary;
use flux_core::LibraryKind;

fn claim(
    e: &Env,
    kinds: &[&str],
    limit: u32,
    lease: Duration,
    at: Option<i64>,
) -> Vec<flux_catalog::JobLease> {
    let mut req = ClaimJobs::new("worker-1", kinds, limit, lease);
    req.now_ms = at;
    e.cat.writer().claim_jobs(req).wait().unwrap()
}

fn job(e: &Env, id: i64) -> jobs::JobInfo {
    e.cat.read_blocking(move |c| jobs::job(c, id)).unwrap().unwrap()
}

#[test]
fn apply_enqueues_match_and_art_and_rescans_do_not_duplicate() {
    let e = env(LibraryKind::Mixed);
    let batch = || {
        vec![
            upsert("Heat (1995).mkv", movie("Heat", Some(1995)), None),
            upsert(
                "Lost/Season 1/Lost - S01E01.mkv",
                episode("Lost", Some(2004), 1, &[1]),
                None,
            ),
            upsert(
                "Lost/Season 1/Lost - S01E02.mkv",
                episode("Lost", Some(2004), 1, &[2]),
                None,
            ),
        ]
    };
    e.apply(batch());
    let counts = e.cat.read_blocking(jobs::job_counts).unwrap();
    let n = |kind: &str| -> u64 {
        counts
            .iter()
            .filter(|c| c.kind == kind && c.state == "queued")
            .map(|c| c.count)
            .sum()
    };
    // match and art for the movie and the show (one art job per work
    // covers its episodes' thumbs).
    assert_eq!((n(JOB_MATCH), n(JOB_ART)), (2, 2), "{counts:?}");

    // The same paths again (a re-parse with fresh stat): nothing new is queued.
    e.apply(batch());
    let total: u64 = e
        .cat
        .read_blocking(jobs::job_counts)
        .unwrap()
        .iter()
        .map(|c| c.count)
        .sum();
    assert_eq!(total, 4);

    // A movie's art waits for its match.
    let first = claim(&e, &[], 10, Duration::from_secs(60), None);
    assert!(first
        .iter()
        .all(|j| j.kind == JOB_MATCH || j.item_id != Some(e.find_top("Heat").id)));
    let heat_match = first
        .iter()
        .find(|j| j.kind == JOB_MATCH && j.item_id == Some(e.find_top("Heat").id))
        .unwrap()
        .clone();
    assert_eq!(heat_match.attempt, 1);
    let w = e.cat.writer();
    assert_eq!(
        w.finish_job(heat_match.id, heat_match.attempt, JobResult::Done)
            .wait()
            .unwrap(),
        Finished::Done
    );
    let next = claim(&e, &[JOB_ART], 10, Duration::from_secs(60), None);
    assert!(
        next.iter().any(|j| j.item_id == Some(e.find_top("Heat").id)),
        "{next:?}"
    );
}

#[test]
fn idempotency_keys_dedupe_and_new_keys_supersede_queued_jobs() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert("Heat (1995).mkv", movie("Heat", Some(1995)), None)]);
    let heat = e.find_top("Heat").id;
    let w = e.cat.writer();
    let spec = |key: &str| {
        JobSpec::new("analyze", key)
            .item(heat)
            .priority(priority::BACKGROUND)
    };
    let a = w.enqueue_job(spec("v1")).wait().unwrap();
    assert!(matches!(a, Enqueued::New(_)));
    assert_eq!(
        w.enqueue_job(spec("v1")).wait().unwrap(),
        Enqueued::Duplicate(a.id())
    );
    // A higher priority for the same key bumps the queued job.
    w.enqueue_job(spec("v1").priority(priority::INTERACTIVE))
        .wait()
        .unwrap();
    assert_eq!(job(&e, a.id()).priority, priority::INTERACTIVE);
    // A new key for the same item replaces the queued job (no second job).
    assert_eq!(
        w.enqueue_job(spec("v2")).wait().unwrap(),
        Enqueued::Superseded(a.id())
    );
    assert_eq!(job(&e, a.id()).key, "v2");
    // Done: the key stays as a record, so enqueueing it again does nothing.
    let l = claim(&e, &["analyze"], 1, Duration::from_secs(60), None).remove(0);
    w.finish_job(l.id, l.attempt, JobResult::Done).wait().unwrap();
    assert_eq!(
        w.enqueue_job(spec("v2")).wait().unwrap(),
        Enqueued::Duplicate(a.id())
    );
    // A changed key after completion is new work; the old record is dropped.
    let c = w.enqueue_job(spec("v3")).wait().unwrap();
    assert!(matches!(c, Enqueued::New(_)));
    let analyze: Vec<String> = e
        .cat
        .read_blocking(move |c| jobs::item_jobs(c, heat))
        .unwrap()
        .into_iter()
        .filter(|j| j.kind == "analyze")
        .map(|j| j.key)
        .collect();
    assert_eq!(analyze, ["v3"]);
    // Item-less jobs dedupe by key alone.
    let x = w
        .enqueue_jobs(vec![JobSpec::new("gc", "daily"), JobSpec::new("gc", "daily")])
        .wait()
        .unwrap();
    assert_eq!(x[1], Enqueued::Duplicate(x[0].id()));
    assert!(w.enqueue_job(JobSpec::new("gc", "")).wait().is_err());
}

#[test]
fn priorities_then_fifo_and_kind_filters() {
    let e = env(LibraryKind::Movies);
    let w = e.cat.writer();
    let ids: Vec<i64> = [("x", "a", 50), ("x", "b", 10), ("y", "c", 0), ("x", "d", 10)]
        .into_iter()
        .map(|(k, key, p)| {
            w.enqueue_job(JobSpec::new(k, key).priority(p))
                .wait()
                .unwrap()
                .id()
        })
        .collect();
    let got = claim(&e, &["x"], 10, Duration::from_secs(60), None);
    assert_eq!(
        got.iter().map(|j| j.id).collect::<Vec<_>>(),
        vec![ids[1], ids[3], ids[0]]
    );
    let got = claim(&e, &[], 10, Duration::from_secs(60), None);
    assert_eq!(got.iter().map(|j| j.key.as_str()).collect::<Vec<_>>(), vec!["c"]);
}

#[test]
fn an_expired_lease_is_reclaimed_and_the_stale_worker_cannot_finish() {
    let e = env(LibraryKind::Movies);
    let w = e.cat.writer();
    let id = w.enqueue_job(JobSpec::new("x", "k")).wait().unwrap().id();
    let t0 = now_ms();
    let first = claim(&e, &["x"], 1, Duration::from_secs(30), Some(t0)).remove(0);
    assert_eq!((first.id, first.attempt), (id, 1));
    assert_eq!(first.lease_until, t0 + 30_000);
    // Leased: nobody else gets it, and the poll hint is the lease end.
    assert!(claim(&e, &["x"], 1, Duration::from_secs(30), Some(t0 + 10_000)).is_empty());
    assert_eq!(
        e.cat.read_blocking(|c| jobs::next_job_due(c, &["x"])).unwrap(),
        Some(t0 + 30_000)
    );
    // A heartbeat extends it.
    assert!(w.extend_job_lease(id, 1, Duration::from_secs(60)).wait().unwrap());
    assert!(claim(&e, &["x"], 1, Duration::from_secs(30), Some(t0 + 40_000)).is_empty());
    // The worker dies. After the lease, the expiry counts as a failed attempt
    // and the job is queued again after the backoff (30 s base).
    let after = now_ms() + 61_000;
    assert!(claim(&e, &["x"], 1, Duration::from_secs(30), Some(after)).is_empty());
    let j = job(&e, id);
    assert_eq!((j.state.as_str(), j.attempts), ("queued", 1));
    assert_eq!(j.last_error.as_deref(), Some("lease expired"));
    let second = claim(&e, &["x"], 1, Duration::from_secs(30), Some(j.not_before)).remove(0);
    assert_eq!(second.attempt, 2);
    // The first worker's late result is discarded.
    assert_eq!(
        w.finish_job(id, 1, JobResult::Done).wait().unwrap(),
        Finished::LeaseLost
    );
    assert!(!w.extend_job_lease(id, 1, Duration::from_secs(60)).wait().unwrap());
    assert_eq!(
        w.finish_job(id, 2, JobResult::Done).wait().unwrap(),
        Finished::Done
    );
    assert_eq!(job(&e, id).state, "done");
}

#[test]
fn retries_back_off_exponentially_until_max_attempts_then_fail() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = CatalogConfig {
        writer: WriterConfig {
            job_max_attempts: 3,
            job_backoff_base: Duration::from_secs(10),
            ..WriterConfig::default()
        },
        ..CatalogConfig::default()
    };
    let cat = Catalog::open(dir.path().join("c.db"), cfg).unwrap();
    cat.writer()
        .create_library(CreateLibrary {
            name: "L".into(),
            kind: LibraryKind::Movies,
            roots: vec!["/m".into()],
        })
        .wait()
        .unwrap();
    let w = cat.writer();
    let id = w.enqueue_job(JobSpec::new("x", "k")).wait().unwrap().id();
    let mut delays = Vec::new();
    let mut at = now_ms();
    for attempt in 1..=3u32 {
        let mut req = ClaimJobs::new("w", &["x"], 1, Duration::from_secs(60));
        req.now_ms = Some(at);
        let l = w.claim_jobs(req).wait().unwrap().remove(0);
        assert_eq!(l.attempt, attempt);
        let before = now_ms();
        match w
            .finish_job(id, attempt, JobResult::Retry(format!("timeout {attempt}")))
            .wait()
            .unwrap()
        {
            Finished::RetryAt(t) => {
                delays.push(t - before);
                at = t;
            }
            Finished::Failed => assert_eq!(attempt, 3, "fails at max_attempts"),
            other => panic!("{other:?}"),
        }
    }
    // 10 s, then 20 s (each + ≤12.5 % jitter, minus clock drift).
    assert_eq!(delays.len(), 2);
    assert!((9_900..=11_300).contains(&delays[0]), "{delays:?}");
    assert!((19_900..=22_600).contains(&delays[1]), "{delays:?}");
    let j = cat.read_blocking(move |c| jobs::job(c, id)).unwrap().unwrap();
    assert_eq!(
        (j.state.as_str(), j.last_error.as_deref()),
        ("failed", Some("timeout 3"))
    );
    // A permanent failure doesn't retry; requeue_failed brings both back.
    let id2 = w.enqueue_job(JobSpec::new("x", "k2")).wait().unwrap().id();
    let mut req = ClaimJobs::new("w", &["x"], 1, Duration::from_secs(60));
    req.now_ms = Some(now_ms() + 1_000_000);
    let l = w.claim_jobs(req).wait().unwrap().remove(0);
    assert_eq!(l.id, id2);
    assert_eq!(
        w.finish_job(id2, 1, JobResult::Fail("no such title".into()))
            .wait()
            .unwrap(),
        Finished::Failed
    );
    assert_eq!(w.requeue_failed_jobs(Some("x".into())).wait().unwrap(), 2);
    let j = cat.read_blocking(move |c| jobs::job(c, id)).unwrap().unwrap();
    assert_eq!((j.state.as_str(), j.attempts), ("queued", 0));
}

#[test]
fn year_adoption_requeues_the_match_with_a_new_key() {
    let e = env(LibraryKind::Movies);
    e.apply(vec![upsert("Arrival.mkv", movie("Arrival", None), None)]);
    let id = e.find_top("Arrival").id;
    let before = e.cat.read_blocking(move |c| jobs::item_jobs(c, id)).unwrap();
    let m = before.iter().find(|j| j.kind == JOB_MATCH).unwrap().clone();
    e.apply(vec![upsert(
        "Arrival (2016).mkv",
        movie("Arrival", Some(2016)),
        None,
    )]);
    let after = e.cat.read_blocking(move |c| jobs::item_jobs(c, id)).unwrap();
    let matches: Vec<_> = after.iter().filter(|j| j.kind == JOB_MATCH).collect();
    assert_eq!(matches.len(), 1, "superseded, not duplicated: {after:?}");
    assert_eq!(matches[0].id, m.id);
    assert!(matches[0].key.contains("|2016|"), "{}", matches[0].key);
}

#[test]
fn new_episodes_rearm_the_show_art_job_once_per_show() {
    let e = env(LibraryKind::Shows);
    e.apply(vec![upsert(
        "Lost/Season 1/Lost - S01E01.mkv",
        episode("Lost", None, 1, &[1]),
        None,
    )]);
    let show = e.find_top("Lost").id;
    let art_jobs = || -> Vec<jobs::JobInfo> {
        e.cat
            .read_blocking(move |c| jobs::item_jobs(c, show))
            .unwrap()
            .into_iter()
            .filter(|j| j.kind == JOB_ART)
            .collect()
    };
    let first = art_jobs();
    assert_eq!(first.len(), 1);
    std::thread::sleep(Duration::from_millis(2));
    // New episodes while the art job is still queued: same job, new key.
    e.apply(vec![
        upsert(
            "Lost/Season 1/Lost - S01E02.mkv",
            episode("Lost", None, 1, &[2]),
            None,
        ),
        upsert(
            "Lost/Season 2/Lost - S02E01.mkv",
            episode("Lost", None, 2, &[1]),
            None,
        ),
    ]);
    let second = art_jobs();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].id, first[0].id);
    assert_ne!(second[0].key, first[0].key);
    // Run match and art to completion; a later episode re-arms a new job.
    let w = e.cat.writer();
    for kind in [JOB_MATCH, JOB_ART] {
        for l in claim(&e, &[kind], 10, Duration::from_secs(60), None) {
            w.finish_job(l.id, l.attempt, JobResult::Done).wait().unwrap();
        }
    }
    assert_eq!(art_jobs()[0].state, "done");
    std::thread::sleep(Duration::from_millis(2));
    e.apply(vec![upsert(
        "Lost/Season 2/Lost - S02E02.mkv",
        episode("Lost", None, 2, &[2]),
        None,
    )]);
    let third = art_jobs();
    assert_eq!(third.len(), 1, "the done record of the older key is dropped");
    assert_eq!(third[0].state, "queued");
    // A rescan of the same files re-arms nothing.
    e.apply(vec![upsert(
        "Lost/Season 2/Lost - S02E02.mkv",
        episode("Lost", None, 2, &[2]),
        None,
    )]);
    assert_eq!(art_jobs()[0].key, third[0].key);
}
