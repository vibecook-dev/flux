//! Serving one accepted connection with hyper.
//!
//! flux does not use `axum::serve` because it cannot set hyper's
//! `half_close`. With the default `half_close(false)`, hyper aborts an
//! in-flight response as soon as it reads EOF from the client. Truffle's
//! sidecar (before the flux/dogfood fix) half-closed the quiet client→server
//! direction of long downloads after its idle timeout, and real clients
//! (`curl` with a closed stdin, some players) half-close too, so a
//! progressive media response was cut mid-movie (research/06 §3.4: 9.4 KB of
//! 62 KB delivered). `half_close(true)` keeps writing the response.
//!
//! Limits per connection (HTTP/1.1 only; h2c prior knowledge is unusable
//! from browsers, so flux doesn't offer it):
//! - **Request heads** must arrive within [`HEADER_READ_TIMEOUT`]. hyper
//!   starts that timer again after every response, so it also closes idle
//!   keep-alive connections.
//! - **Header size** is capped at [`MAX_HEADER_SIZE`] (431 beyond).
//! - **Pipelining:** requests are answered strictly one at a time; hyper
//!   reads ahead at most its ~400 KB buffer, so a pipelining client queues
//!   work only as fast as it reads the answers.
//! - **Writes:** a response the client stopped reading is aborted after the
//!   write timeout ([`crate::limits::WriteTimeout`]).

use std::convert::Infallible;
use std::time::Duration;

use axum::http::Request;
use axum::Router;
use flux_core::api::Identity;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::conn::ConnInfo;
use crate::limits::WriteTimeout;

/// A client that has not sent a complete request head within this long is
/// dropped (slowloris guard; also bounds idle keep-alive connections).
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest request head (request line + headers) accepted.
pub const MAX_HEADER_SIZE: usize = 64 * 1024;

/// The hyper HTTP/1 settings every flux listener uses.
pub fn http1_builder() -> http1::Builder {
    let mut builder = http1::Builder::new();
    builder
        // Required: see the module docs.
        .half_close(true)
        .keep_alive(true)
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .max_header_size(MAX_HEADER_SIZE);
    builder
}

/// Serve HTTP/1.1 on `io` until the client is done or `shutdown` fires (then
/// in-flight requests finish and the connection closes). Every request gets
/// `identity` (as `Option<Identity>`) and `conn` in its extensions, so axum
/// handlers can take `Extension<Option<Identity>>` and `Extension<ConnInfo>`.
/// A response the client stops reading for `write_timeout` ends the
/// connection (`None`: no limit).
pub async fn serve_connection<I>(
    io: I,
    router: Router,
    identity: Option<Identity>,
    conn: ConnInfo,
    shutdown: CancellationToken,
    write_timeout: Option<Duration>,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // A per-request wrapper rather than `router.layer(Extension(..))` per
    // connection: `Router::layer` rebuilds every route, this is two clones.
    let service = tower::service_fn(move |mut req: Request<Incoming>| {
        req.extensions_mut().insert(identity.clone());
        req.extensions_mut().insert(conn.clone());
        let router = router.clone();
        async move { Ok::<_, Infallible>(router.oneshot(req).await.unwrap_or_else(|e| match e {})) }
    });
    let io = WriteTimeout::new(io, write_timeout);
    let connection = http1_builder()
        .serve_connection(TokioIo::new(io), TowerToHyperService::new(service))
        .with_upgrades();
    tokio::pin!(connection);
    tokio::select! {
        result = connection.as_mut() => {
            if let Err(e) = result {
                tracing::debug!(error = %e, "connection ended with an error");
            }
        }
        _ = shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            if let Err(e) = connection.await {
                tracing::debug!(error = %e, "connection ended with an error during shutdown");
            }
        }
    }
}
