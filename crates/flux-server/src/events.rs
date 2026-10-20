//! The event bus behind `GET /api/v1/events` (Server-Sent Events).
//!
//! Sources: the scanner (`ScanProgress`, `ItemsChanged`,
//! `SourceAvailability`), the tailnet listener (`Tailnet`) and the server
//! itself (`ItemsChanged` when background jobs add artwork or metadata).
//!
//! Streams are endless, so they are capped (review SEC-3): at most
//! [`MAX_STREAMS`] at once (past that, `503`, plain text) and
//! [`MAX_STREAMS_PER_IP`] per caller address. A caller at its own cap gets
//! the new stream and its *oldest* stream is ended: that is most likely a
//! closed tab whose close the server hasn't noticed yet (with hyper's
//! `half_close(true)` that takes until a keep-alive write fails, up to
//! ~30 s), and a caller can't use it to take more than its share.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use flux_core::api::Event;
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::ApiError;
use crate::state::{AppState, Caller};

/// Keep-alive comment interval; well under the 30–60 s idle timeouts of
/// proxies and of Truffle's bridge.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Event streams open at once, at most (a browser tab holds one).
pub const MAX_STREAMS: usize = 32;
/// Event streams open at once from one caller address, at most (browsers
/// open at most 6 connections per host anyway).
pub const MAX_STREAMS_PER_IP: usize = 8;

/// Fan-out of [`Event`]s to every SSE client. Slow clients lose old events
/// (they get a `: lagged` comment and should refetch).
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    streams: Arc<Mutex<StreamCounts>>,
}

/// Open event streams, in total and per caller address (`None`: requests
/// that didn't come through flux-net), oldest first.
#[derive(Default)]
struct StreamCounts {
    next_id: u64,
    total: usize,
    per_ip: HashMap<Option<IpAddr>, VecDeque<(u64, CancellationToken)>>,
}

/// One open event stream; dropping it (when the stream ends or the client
/// goes away) frees the slot.
pub struct StreamSlot {
    streams: Arc<Mutex<StreamCounts>>,
    ip: Option<IpAddr>,
    id: u64,
    stop: CancellationToken,
}

impl StreamSlot {
    /// Cancelled when the stream must end: shutdown, or the caller opened a
    /// stream past its cap and this is its oldest.
    pub fn stop(&self) -> CancellationToken {
        self.stop.clone()
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut guard = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        let c = &mut *guard;
        if let Some(q) = c.per_ip.get_mut(&self.ip) {
            // An evicted stream gave its slot back already.
            if let Some(pos) = q.iter().position(|(id, _)| *id == self.id) {
                q.remove(pos);
                c.total -= 1;
            }
            if q.is_empty() {
                c.per_ip.remove(&self.ip);
            }
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        EventBus {
            tx: broadcast::channel(1024).0,
            streams: Arc::default(),
        }
    }
}

impl EventBus {
    pub fn send(&self, event: Event) {
        // No subscribers is fine.
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// A slot for one more event stream from `ip` (ending that caller's
    /// oldest stream if it is at [`MAX_STREAMS_PER_IP`]), or `None` when
    /// [`MAX_STREAMS`] are open. The slot's stop token is a child of
    /// `shutdown`.
    pub fn open_stream(&self, ip: Option<IpAddr>, shutdown: &CancellationToken) -> Option<StreamSlot> {
        let mut guard = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        let c = &mut *guard;
        let mine = c.per_ip.entry(ip).or_default();
        if mine.len() >= MAX_STREAMS_PER_IP {
            if let Some((_, oldest)) = mine.pop_front() {
                oldest.cancel();
                c.total -= 1;
            }
        } else if c.total >= MAX_STREAMS {
            if mine.is_empty() {
                c.per_ip.remove(&ip);
            }
            return None;
        }
        c.next_id += 1;
        let id = c.next_id;
        let stop = shutdown.child_token();
        mine.push_back((id, stop.clone()));
        c.total += 1;
        Some(StreamSlot {
            streams: self.streams.clone(),
            ip,
            id,
            stop,
        })
    }

    /// Event streams open now.
    pub fn open_streams(&self) -> usize {
        self.streams.lock().unwrap_or_else(|p| p.into_inner()).total
    }

    /// Forwards another broadcast source (the scanner) into the bus until
    /// it closes.
    pub fn forward(&self, mut rx: broadcast::Receiver<Event>) -> tokio::task::JoinHandle<()> {
        let bus = self.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(e) => bus.send(e),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(n, "event forwarder lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

/// `GET /api/v1/events`; `503` past [`MAX_STREAMS`].
pub async fn sse(State(st): State<Arc<AppState>>, caller: Caller) -> Response {
    let ip = caller.conn.remote_addr.map(|a| a.ip());
    let Some(slot) = st.events.open_stream(ip, &st.shutdown) else {
        return ApiError::unavailable("Too many open event streams; close another flux tab and reload")
            .into_response();
    };
    let rx = st.events.subscribe();
    let stop = slot.stop();
    // The slot lives in the stream's state: it is freed when the stream ends
    // or the client goes away (the body is dropped).
    let stream = futures::stream::unfold((rx, slot), |(mut rx, slot)| async move {
        let item = match rx.recv().await {
            Ok(event) => {
                let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
                SseEvent::default().data(data)
            }
            Err(broadcast::error::RecvError::Lagged(n)) => SseEvent::default().comment(format!("lagged {n}")),
            Err(broadcast::error::RecvError::Closed) => return None,
        };
        Some((Ok::<_, Infallible>(item), (rx, slot)))
    });
    // End the (otherwise endless) stream on shutdown, so graceful shutdown
    // does not wait for SSE clients, or when the caller's newer stream
    // replaced it.
    let stream = stream.take_until(stop.cancelled_owned());
    // A first comment so clients (and proxies) see the stream open at once.
    let hello = futures::stream::once(async { Ok(SseEvent::default().comment("flux events")) });
    Sse::new(hello.chain(stream))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("keep-alive"))
        .into_response()
}
