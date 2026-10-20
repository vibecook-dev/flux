//! A small priority job queue so that AI never blocks scan or playback.
//!
//! - **Lanes** in strict priority order ([`Lane`]): playback, scan, metadata,
//!   artwork, analysis, then AI (research/05 §6.3). Each lane has its own
//!   concurrency limit, so a full AI lane can never take a scan slot.
//! - **Pausable:** [`JobQueue::pause`] stops a lane from *starting* jobs (call
//!   it for [`Lane::Ai`] when playback begins). Running jobs see the pause via
//!   [`JobContext::paused`] and may yield between steps.
//! - **Budgets:** jobs carry a cost estimate ($, tokens). A lane with a
//!   [`Budget`] reserves the estimate before starting and commits the actual
//!   cost afterwards; when today's $ or token cap would be exceeded the job
//!   waits (it is not dropped) until the UTC day rolls over.
//! - **Idempotency:** a [`JobKey`] is the content fingerprint + job kind +
//!   model id/version + prompt version. Submitting a key that is queued,
//!   running or done is a no-op, so re-scans never pay twice; a model upgrade
//!   changes the key and re-runs only that kind of job.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};

use crate::util::{unix_now, utc_day};

/// Priority lanes, highest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Playback = 0,
    Scan = 1,
    Metadata = 2,
    Artwork = 3,
    Analysis = 4,
    /// LLM, VLM, ASR, embeddings, genAI.
    Ai = 5,
}

impl Lane {
    /// All lanes in priority order.
    pub const ALL: [Lane; 6] = [
        Lane::Playback,
        Lane::Scan,
        Lane::Metadata,
        Lane::Artwork,
        Lane::Analysis,
        Lane::Ai,
    ];
    fn idx(self) -> usize {
        self as usize
    }
}

/// Money and tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub usd: f64,
    pub tokens: u64,
}

impl Cost {
    pub fn usd(usd: f64) -> Self {
        Cost { usd, tokens: 0 }
    }
    pub fn tokens(tokens: u64) -> Self {
        Cost { usd: 0.0, tokens }
    }
    fn add(self, o: Cost) -> Cost {
        Cost {
            usd: self.usd + o.usd,
            tokens: self.tokens + o.tokens,
        }
    }
    fn sub(self, o: Cost) -> Cost {
        Cost {
            usd: (self.usd - o.usd).max(0.0),
            tokens: self.tokens.saturating_sub(o.tokens),
        }
    }
}

/// Why a reservation was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BudgetError {
    #[error("daily $ budget exhausted: ${spent:.4} spent + ${requested:.4} requested > ${limit:.2}")]
    Usd { limit: f64, spent: f64, requested: f64 },
    #[error("daily token budget exhausted: {spent} spent + {requested} requested > {limit}")]
    Tokens { limit: u64, spent: u64, requested: u64 },
}

/// Daily limits (UTC days).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BudgetLimits {
    pub usd_per_day: f64,
    pub tokens_per_day: u64,
}

impl Default for BudgetLimits {
    /// $0.50 and 2M tokens a day: thousands of hard items, a handful of posters.
    fn default() -> Self {
        BudgetLimits {
            usd_per_day: 0.50,
            tokens_per_day: 2_000_000,
        }
    }
}

#[derive(Debug, Default)]
struct BudgetState {
    day: u64,
    spent: Cost,
    reserved: Cost,
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A daily $ / token budget with reservations.
pub struct Budget {
    limits: BudgetLimits,
    state: Mutex<BudgetState>,
    clock: Clock,
}

impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Budget {
    /// A budget on the system clock.
    pub fn new(limits: BudgetLimits) -> Arc<Self> {
        Budget::with_clock(limits, Arc::new(unix_now))
    }

    /// A budget on a custom clock (unix seconds), for tests.
    pub fn with_clock(limits: BudgetLimits, clock: Clock) -> Arc<Self> {
        let day = utc_day(clock());
        Arc::new(Budget {
            limits,
            state: Mutex::new(BudgetState {
                day,
                ..Default::default()
            }),
            clock,
        })
    }

    /// The limits.
    pub fn limits(&self) -> BudgetLimits {
        self.limits
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut BudgetState) -> T) -> T {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let today = utc_day((self.clock)());
        if s.day != today {
            // New UTC day: spending resets; in-flight reservations carry over.
            s.day = today;
            s.spent = Cost::default();
        }
        f(&mut s)
    }

    /// Reserve `estimate` against today's budget.
    pub fn try_reserve(self: &Arc<Self>, estimate: Cost) -> Result<Reservation, BudgetError> {
        let lim = self.limits;
        self.with_state(|s| {
            let used = s.spent.add(s.reserved);
            if used.usd + estimate.usd > lim.usd_per_day + 1e-12 {
                return Err(BudgetError::Usd {
                    limit: lim.usd_per_day,
                    spent: used.usd,
                    requested: estimate.usd,
                });
            }
            if used.tokens + estimate.tokens > lim.tokens_per_day {
                return Err(BudgetError::Tokens {
                    limit: lim.tokens_per_day,
                    spent: used.tokens,
                    requested: estimate.tokens,
                });
            }
            s.reserved = s.reserved.add(estimate);
            Ok(Reservation {
                budget: Arc::clone(self),
                estimate,
                settled: false,
            })
        })
    }

    /// Spent so far today (excluding open reservations).
    pub fn spent_today(&self) -> Cost {
        self.with_state(|s| s.spent)
    }

    /// What is left today (limits minus spent and reserved).
    pub fn remaining_today(&self) -> Cost {
        let lim = self.limits;
        self.with_state(|s| {
            let used = s.spent.add(s.reserved);
            Cost {
                usd: (lim.usd_per_day - used.usd).max(0.0),
                tokens: lim.tokens_per_day.saturating_sub(used.tokens),
            }
        })
    }
}

/// A held reservation. [`Reservation::commit`] records the actual cost; dropping
/// it uncommitted releases the hold (the work never happened or cost nothing).
#[derive(Debug)]
pub struct Reservation {
    budget: Arc<Budget>,
    estimate: Cost,
    settled: bool,
}

impl Reservation {
    /// Record what the work actually cost (may differ from the estimate).
    pub fn commit(mut self, actual: Cost) {
        let est = self.estimate;
        self.budget.with_state(|s| {
            s.reserved = s.reserved.sub(est);
            s.spent = s.spent.add(actual);
        });
        self.settled = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.settled {
            let est = self.estimate;
            self.budget.with_state(|s| s.reserved = s.reserved.sub(est));
        }
    }
}

/// Idempotency key: same content + same job + same model/prompt ⇒ same result.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobKey {
    /// Content fingerprint of the subject (e.g. xxh3-128 of head+tail, or oshash).
    pub content_fp: String,
    /// Job kind, e.g. `"match"`, `"frame_poster"`, `"ai_hypothesis"`, `"genai_poster"`.
    pub kind: String,
    pub model_id: Option<String>,
    pub model_version: Option<String>,
    pub prompt_version: Option<u32>,
}

impl JobKey {
    /// A key for deterministic (non-model) work.
    pub fn new(content_fp: impl Into<String>, kind: impl Into<String>) -> Self {
        JobKey {
            content_fp: content_fp.into(),
            kind: kind.into(),
            model_id: None,
            model_version: None,
            prompt_version: None,
        }
    }

    /// Add the model identity (changing any of these re-runs the job).
    pub fn with_model(
        mut self,
        id: impl Into<String>,
        version: impl Into<String>,
        prompt_version: u32,
    ) -> Self {
        self.model_id = Some(id.into());
        self.model_version = Some(version.into());
        self.prompt_version = Some(prompt_version);
        self
    }
}

impl std::fmt::Display for JobKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{}@{}:p{}",
            self.kind,
            self.content_fp,
            self.model_id.as_deref().unwrap_or("-"),
            self.model_version.as_deref().unwrap_or("-"),
            self.prompt_version.unwrap_or(0)
        )
    }
}

/// Status of a job key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Done { cost: Cost },
    Failed { error: String },
}

/// What [`JobQueue::submit`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum Submitted {
    Accepted,
    /// The key is already queued, running or done; nothing was queued.
    Duplicate(JobStatus),
}

/// Passed to every job.
#[derive(Debug, Clone)]
pub struct JobContext {
    /// Becomes `true` while the job's lane is paused; long jobs should yield.
    pub paused: watch::Receiver<bool>,
}

/// Job body: returns the actual cost (for budget accounting) or an error message.
pub type JobFn = Box<dyn FnOnce(JobContext) -> BoxFuture<'static, Result<Cost, String>> + Send>;

/// Queue configuration.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Max concurrent jobs per lane, indexed like [`Lane::ALL`].
    pub concurrency: [usize; 6],
    /// Budget applied to the AI lane (other lanes are free).
    pub ai_budget: Option<Arc<Budget>>,
    /// How often a budget-blocked lane re-checks (e.g. after midnight UTC).
    pub recheck: Duration,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig {
            concurrency: [8, 4, 4, 2, 2, 1],
            ai_budget: None,
            recheck: Duration::from_secs(60),
        }
    }
}

struct Queued {
    key: JobKey,
    estimate: Cost,
    run: JobFn,
}

struct State {
    queues: [VecDeque<Queued>; 6],
    running: [usize; 6],
    status: HashMap<JobKey, JobStatus>,
}

struct Inner {
    cfg: QueueConfig,
    state: Mutex<State>,
    wake: Notify,
    idle: Notify,
    paused: [watch::Sender<bool>; 6],
}

/// The job queue. Clone to share; call [`JobQueue::start`] once inside a Tokio runtime.
#[derive(Clone)]
pub struct JobQueue {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for JobQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobQueue")
            .field("cfg", &self.inner.cfg)
            .finish_non_exhaustive()
    }
}

impl JobQueue {
    /// A new, idle queue.
    pub fn new(cfg: QueueConfig) -> Self {
        let paused = std::array::from_fn(|_| watch::channel(false).0);
        JobQueue {
            inner: Arc::new(Inner {
                cfg,
                state: Mutex::new(State {
                    queues: Default::default(),
                    running: [0; 6],
                    status: HashMap::new(),
                }),
                wake: Notify::new(),
                idle: Notify::new(),
                paused,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Queue a job unless its key is already queued, running or done.
    pub fn submit(&self, lane: Lane, key: JobKey, estimate: Cost, run: JobFn) -> Submitted {
        {
            let mut s = self.lock();
            // Failed keys may be retried; anything else is a duplicate.
            if let Some(st @ (JobStatus::Queued | JobStatus::Running | JobStatus::Done { .. })) =
                s.status.get(&key)
            {
                return Submitted::Duplicate(st.clone());
            }
            s.status.insert(key.clone(), JobStatus::Queued);
            s.queues[lane.idx()].push_back(Queued { key, estimate, run });
        }
        self.inner.wake.notify_one();
        Submitted::Accepted
    }

    /// Status of a key.
    pub fn status(&self, key: &JobKey) -> Option<JobStatus> {
        self.lock().status.get(key).cloned()
    }

    /// Stop `lane` from starting new jobs (running jobs see `paused = true`).
    pub fn pause(&self, lane: Lane) {
        self.inner.paused[lane.idx()].send_replace(true);
    }

    /// Resume `lane`.
    pub fn resume(&self, lane: Lane) {
        self.inner.paused[lane.idx()].send_replace(false);
        self.inner.wake.notify_one();
    }

    /// Jobs waiting and running in `lane`.
    pub fn depth(&self, lane: Lane) -> (usize, usize) {
        let s = self.lock();
        (s.queues[lane.idx()].len(), s.running[lane.idx()])
    }

    /// Wait until nothing is queued or running (paused or budget-blocked lanes
    /// with queued jobs never become idle; use a timeout).
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            {
                let s = self.lock();
                if s.queues.iter().all(VecDeque::is_empty) && s.running.iter().all(|&r| r == 0) {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Start the dispatcher task.
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let q = self.clone();
        tokio::spawn(async move {
            loop {
                let wake = q.inner.wake.notified();
                while q.dispatch_one() {}
                // Re-check periodically: budgets roll over at midnight UTC.
                let _ = tokio::time::timeout(q.inner.cfg.recheck, wake).await;
                if Arc::strong_count(&q.inner) == 1 {
                    return; // every handle dropped
                }
            }
        })
    }

    /// Start the highest-priority runnable job, if any. Returns whether one started.
    fn dispatch_one(&self) -> bool {
        let mut s = self.lock();
        for lane in Lane::ALL {
            let i = lane.idx();
            if s.queues[i].is_empty()
                || s.running[i] >= self.inner.cfg.concurrency[i].max(1)
                || *self.inner.paused[i].borrow()
            {
                continue;
            }
            let reservation = match (&self.inner.cfg.ai_budget, lane) {
                (Some(b), Lane::Ai) => {
                    let est = s.queues[i].front().map(|j| j.estimate).unwrap_or_default();
                    match b.try_reserve(est) {
                        Ok(r) => Some(r),
                        Err(e) => {
                            tracing::debug!(error = %e, "AI lane budget-blocked");
                            continue; // stays queued; lower lanes still run
                        }
                    }
                }
                _ => None,
            };
            let Some(job) = s.queues[i].pop_front() else {
                continue;
            };
            s.running[i] += 1;
            s.status.insert(job.key.clone(), JobStatus::Running);
            drop(s);
            let q = self.clone();
            let ctx = JobContext {
                paused: self.inner.paused[i].subscribe(),
            };
            tokio::spawn(async move {
                let result = (job.run)(ctx).await;
                let status = match result {
                    Ok(cost) => {
                        if let Some(r) = reservation {
                            r.commit(cost);
                        }
                        JobStatus::Done { cost }
                    }
                    Err(error) => JobStatus::Failed { error },
                };
                {
                    let mut s = q.lock();
                    s.running[i] -= 1;
                    s.status.insert(job.key, status);
                }
                q.inner.wake.notify_one();
                q.inner.idle.notify_waiters();
            });
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    fn job(f: impl FnOnce() -> Result<Cost, String> + Send + 'static) -> JobFn {
        Box::new(move |_ctx| Box::pin(async move { f() }))
    }

    #[test]
    fn budget_reserve_commit_release_and_rollover() {
        let now = Arc::new(AtomicU64::new(20_744 * 86_400 + 3600));
        let n2 = Arc::clone(&now);
        let b = Budget::with_clock(
            BudgetLimits {
                usd_per_day: 0.10,
                tokens_per_day: 1000,
            },
            Arc::new(move || n2.load(Ordering::SeqCst)),
        );
        let r1 = b.try_reserve(Cost::usd(0.06)).unwrap();
        assert!(matches!(
            b.try_reserve(Cost::usd(0.06)),
            Err(BudgetError::Usd { .. })
        ));
        drop(r1); // released, nothing spent
        let r2 = b
            .try_reserve(Cost {
                usd: 0.06,
                tokens: 600,
            })
            .unwrap();
        r2.commit(Cost {
            usd: 0.05,
            tokens: 500,
        });
        assert!((b.spent_today().usd - 0.05).abs() < 1e-9);
        assert!(matches!(
            b.try_reserve(Cost::tokens(600)),
            Err(BudgetError::Tokens { .. })
        ));
        // Next UTC day: spending resets.
        now.fetch_add(86_400, Ordering::SeqCst);
        assert_eq!(b.spent_today(), Cost::default());
        assert!(b.try_reserve(Cost::usd(0.09)).is_ok());
    }

    #[test]
    fn job_keys_include_model_identity() {
        let a = JobKey::new("fp1", "ai_hypothesis").with_model("qwen3.5-4b", "q4", 1);
        let b = JobKey::new("fp1", "ai_hypothesis").with_model("qwen3.5-4b", "q4", 2);
        assert_ne!(a, b);
        assert_eq!(a.to_string(), "ai_hypothesis:fp1:qwen3.5-4b@q4:p1");
    }

    #[tokio::test]
    async fn priority_dedupe_and_lane_isolation() {
        let q = JobQueue::new(QueueConfig {
            concurrency: [1, 1, 1, 1, 1, 1],
            ..Default::default()
        });
        let order = Arc::new(Mutex::new(Vec::new()));
        // Queue before starting so priority, not arrival, decides.
        for (lane, name) in [
            (Lane::Ai, "ai"),
            (Lane::Analysis, "analysis"),
            (Lane::Scan, "scan"),
            (Lane::Playback, "play"),
        ] {
            let o = Arc::clone(&order);
            let r = q.submit(
                lane,
                JobKey::new(name, "t"),
                Cost::default(),
                job(move || {
                    o.lock().unwrap().push(name);
                    Ok(Cost::default())
                }),
            );
            assert_eq!(r, Submitted::Accepted);
        }
        // Same key again: deduplicated.
        assert!(matches!(
            q.submit(
                Lane::Ai,
                JobKey::new("ai", "t"),
                Cost::default(),
                job(|| Ok(Cost::default()))
            ),
            Submitted::Duplicate(JobStatus::Queued)
        ));
        q.start();
        tokio::time::timeout(Duration::from_secs(5), q.wait_idle())
            .await
            .unwrap();
        let o = order.lock().unwrap().clone();
        assert_eq!(o.len(), 4);
        // Each lane has its own slot, so all start in one dispatch round, highest priority first.
        assert_eq!(o[0], "play");
        assert!(matches!(
            q.status(&JobKey::new("ai", "t")),
            Some(JobStatus::Done { .. })
        ));
        // Done keys stay deduplicated (re-scans never pay twice).
        assert!(matches!(
            q.submit(
                Lane::Ai,
                JobKey::new("ai", "t"),
                Cost::default(),
                job(|| Ok(Cost::default()))
            ),
            Submitted::Duplicate(JobStatus::Done { .. })
        ));
    }

    #[tokio::test]
    async fn paused_and_budget_blocked_ai_never_blocks_other_lanes() {
        let budget = Budget::new(BudgetLimits {
            usd_per_day: 0.05,
            tokens_per_day: 1_000_000,
        });
        let q = JobQueue::new(QueueConfig {
            ai_budget: Some(Arc::clone(&budget)),
            ..Default::default()
        });
        q.start();
        let ran = Arc::new(AtomicUsize::new(0));
        // Two AI jobs at $0.04: the second exceeds today's $0.05 and must wait.
        for i in 0..2 {
            let r = Arc::clone(&ran);
            q.submit(
                Lane::Ai,
                JobKey::new(format!("p{i}"), "genai"),
                Cost::usd(0.04),
                job(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                    Ok(Cost::usd(0.04))
                }),
            );
        }
        // Pause AI (playback started), then a scan job must still run.
        tokio::time::sleep(Duration::from_millis(50)).await;
        q.pause(Lane::Ai);
        let scan_ran = Arc::new(AtomicUsize::new(0));
        let s2 = Arc::clone(&scan_ran);
        q.submit(
            Lane::Scan,
            JobKey::new("f", "probe"),
            Cost::default(),
            job(move || {
                s2.fetch_add(1, Ordering::SeqCst);
                Ok(Cost::default())
            }),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(scan_ran.load(Ordering::SeqCst), 1);
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "budget must hold the second AI job"
        );
        assert_eq!(q.depth(Lane::Ai), (1, 0));
        assert!((budget.spent_today().usd - 0.04).abs() < 1e-9);
        q.resume(Lane::Ai);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ran.load(Ordering::SeqCst), 1, "still over budget after resume");
    }
}
