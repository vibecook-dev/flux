//! Concurrency primitives: the shared worker pool (a priority queue in front
//! of `workers` threads), a per-scan in-flight counter that bounds the
//! walk → worker hand-off, and a priority-ordered counting semaphore for
//! per-device file I/O.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

type Job = Box<dyn FnOnce() + Send + 'static>;

struct Task {
    prio: u8,
    seq: u64,
    job: Job,
}

impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool {
        self.prio == other.prio && self.seq == other.seq
    }
}
impl Eq for Task {}
impl PartialOrd for Task {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Task {
    /// Max-heap: higher priority first, then FIFO.
    fn cmp(&self, other: &Self) -> Ordering {
        self.prio.cmp(&other.prio).then_with(|| other.seq.cmp(&self.seq))
    }
}

struct Queue {
    heap: BinaryHeap<Task>,
    seq: u64,
    stop: bool,
}

struct PoolShared {
    q: Mutex<Queue>,
    cv: Condvar,
}

/// `n` worker threads draining one priority queue. All scans share it, so a
/// watcher batch's files are processed before a scheduled scan's backlog.
pub(crate) struct WorkerPool {
    shared: Arc<PoolShared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl WorkerPool {
    pub(crate) fn new(n: usize) -> WorkerPool {
        let shared = Arc::new(PoolShared {
            q: Mutex::new(Queue {
                heap: BinaryHeap::new(),
                seq: 0,
                stop: false,
            }),
            cv: Condvar::new(),
        });
        let threads = (0..n.max(1))
            .filter_map(|i| {
                let s = shared.clone();
                std::thread::Builder::new()
                    .name(format!("flux-scan-worker-{i}"))
                    .spawn(move || worker_main(&s))
                    .ok()
            })
            .collect();
        WorkerPool {
            shared,
            threads: Mutex::new(threads),
        }
    }

    /// Queue `job` at `prio` (higher runs first). Returns `false` once the
    /// pool is stopping (the job is dropped).
    pub(crate) fn submit(&self, prio: u8, job: impl FnOnce() + Send + 'static) -> bool {
        let mut q = lock(&self.shared.q);
        if q.stop {
            return false;
        }
        q.seq += 1;
        let seq = q.seq;
        q.heap.push(Task {
            prio,
            seq,
            job: Box::new(job),
        });
        drop(q);
        self.shared.cv.notify_one();
        true
    }

    /// Stop accepting work, let the threads finish queued tasks, join them.
    pub(crate) fn stop(&self) {
        lock(&self.shared.q).stop = true;
        self.shared.cv.notify_all();
        let threads = std::mem::take(&mut *lock(&self.threads));
        for t in threads {
            let _ = t.join();
        }
    }
}

impl Drop for WorkerPool {
    /// Stop without joining: the last handle may be dropped on a worker.
    fn drop(&mut self) {
        lock(&self.shared.q).stop = true;
        self.shared.cv.notify_all();
    }
}

/// A worker that ran tasks and then found no work for this long hands its
/// allocator caches back ([`flux_catalog::release_thread_memory`]): with a
/// thread-caching allocator a scan's per-file buffers would otherwise stay
/// with the idle threads (measured in docs/prototype/server.md, Phase E).
const IDLE_RELEASE: Duration = Duration::from_secs(1);

fn worker_main(s: &PoolShared) {
    // Ran a task since the last release.
    let mut used = false;
    loop {
        let task = {
            let mut q = lock(&s.q);
            loop {
                if let Some(t) = q.heap.pop() {
                    break Some(t);
                }
                if q.stop {
                    break None;
                }
                if !used {
                    q = s.cv.wait(q).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                let (g, waited) =
                    s.cv.wait_timeout(q, IDLE_RELEASE)
                        .unwrap_or_else(|e| e.into_inner());
                q = g;
                if waited.timed_out() && q.heap.is_empty() && !q.stop {
                    drop(q);
                    used = false;
                    flux_catalog::release_thread_memory();
                    q = lock(&s.q);
                }
            }
        };
        let Some(task) = task else { return };
        used = true;
        // A panicking task must not take a worker down with it.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task.job));
    }
}

/// Counts outstanding work of one scan: the walk waits while too much is in
/// flight (back-pressure), and the scan waits for zero before reconciling.
#[derive(Default)]
pub(crate) struct Pending {
    n: Mutex<usize>,
    cv: Condvar,
}

impl Pending {
    pub(crate) fn add(&self, k: usize) {
        *lock(&self.n) += k;
    }

    pub(crate) fn done(&self) {
        let mut n = lock(&self.n);
        *n = n.saturating_sub(1);
        drop(n);
        self.cv.notify_all();
    }

    /// Blocks while more than `cap` are outstanding (or until `stop()`).
    pub(crate) fn wait_below(&self, cap: usize, stop: impl Fn() -> bool) {
        let mut n = lock(&self.n);
        while *n > cap && !stop() {
            n = self
                .cv
                .wait_timeout(n, Duration::from_millis(50))
                .map(|r| r.0)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }

    pub(crate) fn wait_zero(&self) {
        self.wait_below(0, || false);
    }

    #[cfg(test)]
    pub(crate) fn get(&self) -> usize {
        *lock(&self.n)
    }
}

/// A counting semaphore whose waiters are served highest priority first.
pub(crate) struct PrioritySemaphore {
    st: Mutex<SemState>,
    cv: Condvar,
}

struct SemState {
    free: usize,
    seq: u64,
    /// (priority, seq) of waiters.
    waiting: BinaryHeap<(u8, std::cmp::Reverse<u64>)>,
}

pub(crate) struct SemPermit<'a> {
    sem: &'a PrioritySemaphore,
}

impl Drop for SemPermit<'_> {
    fn drop(&mut self) {
        lock(&self.sem.st).free += 1;
        self.sem.cv.notify_all();
    }
}

impl PrioritySemaphore {
    pub(crate) fn new(permits: usize) -> Self {
        PrioritySemaphore {
            st: Mutex::new(SemState {
                free: permits.max(1),
                seq: 0,
                waiting: BinaryHeap::new(),
            }),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn acquire(&self, prio: u8) -> SemPermit<'_> {
        let mut st = lock(&self.st);
        if st.free > 0 && st.waiting.is_empty() {
            st.free -= 1;
            return SemPermit { sem: self };
        }
        st.seq += 1;
        let me = (prio, std::cmp::Reverse(st.seq));
        st.waiting.push(me);
        loop {
            if st.free > 0 && st.waiting.peek() == Some(&me) {
                st.waiting.pop();
                st.free -= 1;
                drop(st);
                // Others may be next in line for remaining permits.
                self.cv.notify_all();
                return SemPermit { sem: self };
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AO};

    #[test]
    fn pool_runs_higher_priority_first() {
        // One worker, blocked on a gate while we queue mixed priorities.
        let pool = WorkerPool::new(1);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let g = gate.clone();
        pool.submit(0, move || {
            let mut open = g.0.lock().unwrap();
            while !*open {
                open = g.1.wait(open).unwrap();
            }
        });
        let order = Arc::new(Mutex::new(Vec::new()));
        for (p, tag) in [
            (0u8, "deep"),
            (2, "watch"),
            (1, "sched"),
            (3, "inter"),
            (2, "watch2"),
        ] {
            let o = order.clone();
            pool.submit(p, move || o.lock().unwrap().push(tag));
        }
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        pool.stop();
        assert_eq!(
            *order.lock().unwrap(),
            vec!["inter", "watch", "watch2", "sched", "deep"]
        );
    }

    /// Threads that ran the registered release hook, with a count each.
    static RELEASED: Mutex<Vec<std::thread::ThreadId>> = Mutex::new(Vec::new());

    fn record_release() {
        lock(&RELEASED).push(std::thread::current().id());
    }

    /// A worker hands its allocator caches back once after going idle, and
    /// not again until it has run another task.
    #[test]
    fn idle_workers_release_memory_once() {
        // Other tests of this binary may register or trigger releases too:
        // only this pool's worker threads are counted.
        flux_catalog::set_release_hook(record_release);
        let pool = WorkerPool::new(2);
        let ids = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(std::sync::Barrier::new(3));
        for _ in 0..2 {
            let (ids, gate) = (ids.clone(), gate.clone());
            // Both workers run one task (the barrier holds each until both have one).
            pool.submit(0, move || {
                lock(&ids).push(std::thread::current().id());
                gate.wait();
            });
        }
        gate.wait();
        let ids = lock(&ids).clone();
        assert_eq!(ids.len(), 2);
        let count = || {
            let r = lock(&RELEASED);
            ids.iter()
                .map(|id| r.iter().filter(|x| *x == id).count())
                .collect::<Vec<_>>()
        };
        let t0 = std::time::Instant::now();
        while count() != vec![1, 1] && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(count(), vec![1, 1], "each worker released once after going idle");
        std::thread::sleep(IDLE_RELEASE * 2);
        assert_eq!(count(), vec![1, 1], "no repeated release while idle");
        pool.stop();
    }

    #[test]
    fn pending_bounds_and_drains() {
        let p = Arc::new(Pending::default());
        p.add(3);
        let q = p.clone();
        let h = std::thread::spawn(move || {
            for _ in 0..3 {
                std::thread::sleep(Duration::from_millis(5));
                q.done();
            }
        });
        p.wait_below(1, || false);
        assert!(p.get() <= 1);
        p.wait_zero();
        assert_eq!(p.get(), 0);
        h.join().unwrap();
    }

    #[test]
    fn semaphore_limits_concurrency() {
        let sem = Arc::new(PrioritySemaphore::new(2));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let (s, l, p) = (sem.clone(), live.clone(), peak.clone());
                std::thread::spawn(move || {
                    let _permit = s.acquire((i % 4) as u8);
                    let now = l.fetch_add(1, AO::SeqCst) + 1;
                    p.fetch_max(now, AO::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    l.fetch_sub(1, AO::SeqCst);
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert!(peak.load(AO::SeqCst) <= 2);
    }
}
