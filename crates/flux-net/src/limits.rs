//! What one caller can hold open: connection admission per remote IP and in
//! total, and a deadline for responses the client has stopped reading.
//!
//! Without these, a few hundred idle connections (or SSE streams) from one
//! LAN or tailnet caller exhausted the process's file descriptors and wedged
//! every listener (review SEC-3), and each client that stopped reading a
//! media response pinned its buffers and connection forever (SEC-6).
//!
//! - **Admission** happens right after `accept`, before any HTTP: over a
//!   limit the connection gets a best-effort `503` and is closed at once, so
//!   it neither waits in the backlog nor costs a task.
//! - **Loopback has its own budget.** Loopback connections count against
//!   the per-IP limit but not the total, so a LAN flood can't lock the owner
//!   out of their own UI on `127.0.0.1`.
//! - **Write timeout:** see [`WriteTimeout`].

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::conn::canonical_ip;

/// Limits on what callers can hold open (see the module docs). The
/// defaults suit a home server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeLimits {
    /// Connections open at once from non-loopback callers, over all
    /// listeners together. flux-server lowers it to fit the process's file
    /// descriptor limit.
    pub max_connections: usize,
    /// Connections open at once from one remote IP (loopback included).
    pub max_connections_per_ip: usize,
    /// A response whose client accepts no bytes for this long is aborted:
    /// its connection is closed and its buffers freed. `None` waits forever.
    /// 60 s, like nginx's `send_timeout`; a paused player that stops
    /// reading reconnects with a `Range` request when it resumes.
    pub write_timeout: Option<Duration>,
}

impl Default for ServeLimits {
    fn default() -> Self {
        ServeLimits {
            max_connections: 512,
            max_connections_per_ip: 64,
            write_timeout: Some(Duration::from_secs(60)),
        }
    }
}

/// Counts open connections and admits new ones within [`ServeLimits`].
pub(crate) struct Admission {
    limits: ServeLimits,
    counts: Mutex<Counts>,
    /// Unix seconds of the last "refusing connections" warning (one a minute).
    last_warn_s: AtomicU64,
}

#[derive(Default)]
struct Counts {
    /// Non-loopback connections.
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// One admitted connection; dropping it (when the connection's task ends)
/// gives its slot back.
pub(crate) struct Permit {
    admission: Arc<Admission>,
    ip: IpAddr,
    counted: bool,
}

impl Admission {
    pub(crate) fn new(limits: ServeLimits) -> Arc<Admission> {
        Arc::new(Admission {
            limits,
            counts: Mutex::new(Counts::default()),
            last_warn_s: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Counts> {
        // Plain counters: a panic elsewhere can't leave them invalid.
        self.counts.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A slot for a connection from `ip`, or `None` when that caller (or
    /// everyone together) is at the limit.
    pub(crate) fn try_admit(self: &Arc<Self>, ip: IpAddr) -> Option<Permit> {
        let ip = canonical_ip(ip);
        let counted = !ip.is_loopback();
        let mut c = self.lock();
        let mine = c.per_ip.get(&ip).copied().unwrap_or(0);
        if mine >= self.limits.max_connections_per_ip || (counted && c.total >= self.limits.max_connections) {
            drop(c);
            self.warn_refused(ip);
            return None;
        }
        *c.per_ip.entry(ip).or_default() += 1;
        if counted {
            c.total += 1;
        }
        Some(Permit {
            admission: self.clone(),
            ip,
            counted,
        })
    }

    fn warn_refused(&self, ip: IpAddr) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let last = self.last_warn_s.load(Ordering::Relaxed);
        if now >= last + 60
            && self
                .last_warn_s
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                %ip,
                max = self.limits.max_connections,
                per_ip = self.limits.max_connections_per_ip,
                "refusing connections: too many open (logged once a minute)"
            );
        } else {
            tracing::debug!(%ip, "connection refused: too many open");
        }
    }

    /// (non-loopback total, connections from `ip`).
    #[cfg(test)]
    pub(crate) fn open(&self, ip: IpAddr) -> (usize, usize) {
        let c = self.lock();
        (c.total, c.per_ip.get(&canonical_ip(ip)).copied().unwrap_or(0))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut c = self.admission.lock();
        if let Some(n) = c.per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                c.per_ip.remove(&self.ip);
            }
        }
        if self.counted {
            c.total -= 1;
        }
    }
}

/// What a refused connection is told before it is closed.
const BUSY: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Length: 41\r\n\
Retry-After: 5\r\n\
Connection: close\r\n\r\n\
Too many connections; try again shortly.\n";

/// Tells a connection over the limits so (one non-blocking write; the
/// response fits any fresh socket's send buffer), then closes it. The write
/// goes through the std socket: tokio's `try_write` reports `WouldBlock`
/// until the reactor has seen the new socket writable.
pub(crate) fn refuse(stream: tokio::net::TcpStream) {
    use std::io::Write;
    if let Ok(mut socket) = stream.into_std() {
        let _ = socket.write(BUSY);
    }
}

/// `io` with a write deadline. While a write (or flush) makes no progress
/// because the peer stopped reading, a timer runs; when it reaches the
/// limit, writes fail with [`io::ErrorKind::TimedOut`]. hyper then ends the
/// connection, which drops the response body and its buffers. Any progress
/// resets the timer, so a slow but reading client is never cut; nor is a
/// response that is merely waiting for its body (an SSE stream between
/// events), because nothing is being written then.
pub struct WriteTimeout<I> {
    io: I,
    limit: Option<Duration>,
    /// Created on the first stall, then reset for each new one.
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    stalled: bool,
}

impl<I> WriteTimeout<I> {
    /// Wraps `io`; `limit: None` passes everything through.
    pub fn new(io: I, limit: Option<Duration>) -> Self {
        WriteTimeout {
            io,
            limit,
            sleep: None,
            stalled: false,
        }
    }

    fn track<T>(&mut self, cx: &mut Context<'_>, r: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        let Some(limit) = self.limit else { return r };
        if r.is_ready() {
            self.stalled = false;
            return r;
        }
        if !self.stalled {
            // A new stall: the clock starts now.
            let deadline = tokio::time::Instant::now() + limit;
            match self.sleep.as_mut() {
                Some(s) => s.as_mut().reset(deadline),
                None => self.sleep = Some(Box::pin(tokio::time::sleep_until(deadline))),
            }
            self.stalled = true;
        }
        let Some(sleep) = self.sleep.as_mut() else {
            return Poll::Pending;
        };
        // Registers the waker, so the task is polled again at the deadline.
        match sleep.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the client stopped reading the response",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for WriteTimeout<I> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for WriteTimeout<I> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_write(cx, buf);
        this.track(cx, r)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.track(cx, r)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_flush(cx);
        this.track(cx, r)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_shutdown(cx);
        this.track(cx, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn per_ip_and_total_limits_with_a_separate_loopback_budget() {
        let adm = Admission::new(ServeLimits {
            max_connections: 3,
            max_connections_per_ip: 2,
            write_timeout: None,
        });
        let a1 = adm.try_admit(ip("192.168.1.10")).expect("first");
        let a2 = adm.try_admit(ip("192.168.1.10")).expect("second");
        assert!(adm.try_admit(ip("192.168.1.10")).is_none(), "per-IP limit");
        // IPv4-mapped IPv6 is the same caller.
        assert!(adm.try_admit(ip("::ffff:192.168.1.10")).is_none());
        let b1 = adm.try_admit(ip("192.168.1.11")).expect("another caller");
        assert!(adm.try_admit(ip("192.168.1.12")).is_none(), "total limit");
        // Loopback doesn't count against (or get blocked by) the total…
        let l1 = adm
            .try_admit(ip("127.0.0.1"))
            .expect("loopback has its own budget");
        let l2 = adm.try_admit(ip("127.0.0.1")).expect("loopback");
        // …but has the per-IP limit.
        assert!(adm.try_admit(ip("127.0.0.1")).is_none());
        assert_eq!(adm.open(ip("127.0.0.1")), (3, 2));
        // Slots come back when connections end.
        drop(a1);
        assert_eq!(adm.open(ip("192.168.1.10")), (2, 1));
        let c1 = adm.try_admit(ip("192.168.1.12")).expect("a slot was freed");
        drop((a2, b1, c1, l1, l2));
        assert_eq!(adm.open(ip("192.168.1.10")), (0, 0));
        assert!(adm.lock().per_ip.is_empty(), "no entries left behind");
    }

    #[test]
    fn busy_response_is_well_formed() {
        let text = std::str::from_utf8(BUSY).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 503 "));
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(len, body.len());
    }

    /// A writer that is never ready, like a socket whose peer stopped reading.
    struct Stuck;

    impl AsyncRead for Stuck {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for Stuck {
        fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_write_fails_after_the_limit() {
        use tokio::io::AsyncWriteExt;
        let mut io = WriteTimeout::new(Stuck, Some(Duration::from_secs(60)));
        let t0 = tokio::time::Instant::now();
        let err = io.write_all(b"segment bytes").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(t0.elapsed(), Duration::from_secs(60));

        let mut forever = WriteTimeout::new(Stuck, None);
        let r = tokio::time::timeout(Duration::from_secs(3600), forever.write_all(b"x")).await;
        assert!(r.is_err(), "no limit: the write just waits");
    }
}
