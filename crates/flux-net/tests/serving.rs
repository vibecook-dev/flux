//! Loopback tests of flux-net's listeners: identity extensions, the
//! half-close behaviour long media responses depend on, graceful shutdown,
//! connection admission and the write timeout. No Tailscale involved.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::header;
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Json, Router};
use bytes::Bytes;
use flux_core::api::{Identity, Role};
use flux_net::{ConnInfo, FluxNet, FluxNetConfig, RolePolicy};
#[cfg(unix)]
use flux_net::{SystemConfig, TailnetConfig, TailnetStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Long-stream shape from research/06 §3.4, shortened: CHUNKS × 1 KiB, one
/// chunk every INTERVAL, with an exact Content-Length.
const CHUNKS: usize = 40;
const CHUNK: usize = 1024;
const INTERVAL: Duration = Duration::from_millis(20);

fn router() -> Router {
    Router::new()
        .route(
            "/whoami",
            get(|Extension(you): Extension<Option<Identity>>, Extension(conn): Extension<ConnInfo>| async move {
                Json(serde_json::json!({ "you": you, "path": conn.path.as_str() }))
            }),
        )
        .route("/stream", get(|| async { slow_stream() }))
}

/// A response that trickles out like progressive media to a slow player.
fn slow_stream() -> Response {
    let chunks = futures::stream::unfold(0usize, |i| async move {
        if i == CHUNKS {
            return None;
        }
        tokio::time::sleep(INTERVAL).await;
        Some((
            Ok::<_, Infallible>(Bytes::from(vec![b'a' + (i % 26) as u8; CHUNK])),
            i + 1,
        ))
    });
    Response::builder()
        .header(header::CONTENT_LENGTH, CHUNKS * CHUNK)
        .header(header::CONTENT_TYPE, "video/x-matroska")
        .body(Body::from_stream(chunks))
        .unwrap()
}

/// GET over a fresh connection; returns (status, body).
async fn get_body(stream: &mut TcpStream, path: &str, close_write_after_request: bool) -> (u16, Vec<u8>) {
    let req = format!("GET {path} HTTP/1.1\r\nHost: flux\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    if close_write_after_request {
        // The client half-closes, as the sidecar's old per-direction idle
        // reap did to the quiet request half of a long download.
        stream.shutdown().await.unwrap();
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await; // a reset just ends the body early
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return (0, Vec::new());
    };
    let status = std::str::from_utf8(&raw[..split])
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, raw[split + 4..].to_vec())
}

async fn whoami(addr: SocketAddr) -> serde_json::Value {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let (status, body) = get_body(&mut stream, "/whoami", false).await;
    assert_eq!(status, 200);
    serde_json::from_slice(&body).unwrap()
}

fn local_config() -> FluxNetConfig {
    FluxNetConfig {
        local_addr: "127.0.0.1:0".parse().unwrap(),
        roles: RolePolicy {
            owner_login: Some("owner@example.com".into()),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn localhost_is_the_owner() {
    let net = FluxNet::start(local_config(), router()).await.unwrap();
    let v = whoami(net.local_addr()).await;
    assert_eq!(v["path"], "localhost");
    assert_eq!(v["you"]["login_name"], "owner@example.com");
    assert_eq!(v["you"]["role"], serde_json::to_value(Role::Owner).unwrap());
    net.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn untrusted_localhost_is_anonymous() {
    let mut config = local_config();
    config.roles.localhost_role = None;
    let net = FluxNet::start(config, router()).await.unwrap();
    let v = whoami(net.local_addr()).await;
    assert!(v["you"].is_null());
    net.shutdown(Duration::from_secs(1)).await;
}

/// The address this host would use to reach the outside world (a UDP
/// "connect" sends nothing). `None` without a non-loopback interface.
fn non_loopback_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[tokio::test]
async fn lan_callers_have_no_identity() {
    let Some(ip) = non_loopback_ip() else {
        eprintln!("skipped: no non-loopback interface");
        return;
    };
    let mut config = local_config();
    config.lan_addr = Some(SocketAddr::new(ip, 0));
    let net = FluxNet::start(config, router()).await.unwrap();
    let lan = net.lan_addr().unwrap();
    let v = whoami(lan).await;
    assert_eq!(v["path"], "lan", "{v}");
    assert!(v["you"].is_null(), "LAN callers are anonymous: {v}");
    net.shutdown(Duration::from_secs(1)).await;
}

/// The regression that motivates serving with hyper directly: a long
/// response must survive the client closing its write half (research/06
/// §3.4 — `axum::serve` delivered 9.4 KB of 62 KB).
#[tokio::test]
async fn long_stream_survives_client_half_close() {
    let net = FluxNet::start(local_config(), router()).await.unwrap();
    let mut stream = TcpStream::connect(net.local_addr()).await.unwrap();
    let started = Instant::now();
    let (status, body) = get_body(&mut stream, "/stream", true).await;
    assert_eq!(status, 200);
    assert_eq!(
        body.len(),
        CHUNKS * CHUNK,
        "the full body must arrive after a client half-close"
    );
    assert!(body
        .chunks(CHUNK)
        .enumerate()
        .all(|(i, c)| c.iter().all(|&b| b == b'a' + (i % 26) as u8)));
    assert!(
        started.elapsed() >= INTERVAL * (CHUNKS as u32 - 1),
        "the stream really was long-lived"
    );
    net.shutdown(Duration::from_secs(1)).await;
}

/// Control for the test above: the same router behind hyper's default
/// `half_close(false)` aborts the response, so the test above is sensitive
/// to the setting it guards.
#[tokio::test]
async fn control_default_hyper_truncates_after_half_close() {
    use hyper_util::rt::TokioIo;
    use hyper_util::service::TowerToHyperService;
    use tower::ServiceExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let svc = tower::service_fn(|mut req: axum::http::Request<hyper::body::Incoming>| {
            req.extensions_mut().insert(None::<Identity>);
            async move { Ok::<_, Infallible>(router().oneshot(req).await.unwrap_or_else(|e| match e {})) }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), TowerToHyperService::new(svc))
            .await;
    });
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let (_, body) = get_body(&mut stream, "/stream", true).await;
    assert!(
        body.len() < CHUNKS * CHUNK,
        "hyper's default half_close(false) now delivers the whole body ({} bytes); \
         the half-close test no longer proves anything",
        body.len()
    );
}

/// Shutdown lets an in-flight response finish, then closes.
#[tokio::test]
async fn graceful_shutdown_completes_in_flight_responses() {
    let net = FluxNet::start(local_config(), router()).await.unwrap();
    let addr = net.local_addr();
    let download = tokio::spawn(async move {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        get_body(&mut stream, "/stream", false).await
    });
    tokio::time::sleep(INTERVAL * 5).await;
    net.shutdown(Duration::from_secs(5)).await;
    let (status, body) = download.await.unwrap();
    assert_eq!((status, body.len()), (200, CHUNKS * CHUNK));
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "listener closed after shutdown"
    );
}

/// Review SEC-3: one caller holding many idle connections (the reviewer's
/// SSE flood) used to exhaust the process's file descriptors and wedge every
/// listener. Now a caller at its per-IP limit gets `503` at once, the
/// connections it holds keep working, and a slot frees when one closes.
#[tokio::test]
async fn connections_over_the_per_ip_limit_are_refused_at_once() {
    let mut config = local_config();
    config.limits.max_connections_per_ip = 4;
    let net = FluxNet::start(config, router()).await.unwrap();
    let addr = net.local_addr();
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(TcpStream::connect(addr).await.unwrap());
    }
    // The fifth is answered without sending anything, and closed.
    let mut extra = TcpStream::connect(addr).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), extra.read_to_end(&mut raw))
        .await
        .expect("refused at once, not left waiting")
        .unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 503 "), "{text}");
    assert!(text.contains("Too many connections"), "{text}");

    // Admitted connections are served as usual.
    let (status, _) = get_body(&mut held[0], "/whoami", false).await;
    assert_eq!(status, 200);
    // That one is closed now (Connection: close): its slot comes back.
    drop(held.remove(0));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut again = TcpStream::connect(addr).await.unwrap();
        let (status, _) = get_body(&mut again, "/whoami", false).await;
        if status == 200 {
            break;
        }
        assert!(Instant::now() < deadline, "the freed slot never came back");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(held);
    net.shutdown(Duration::from_secs(1)).await;
}

/// Sets its flag when dropped (here: when the response body is dropped).
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// An endless 64 KiB-chunk body that reports when it is dropped.
fn endless(dropped: Arc<AtomicBool>) -> Router {
    Router::new().route(
        "/endless",
        get(move || {
            let flag = DropFlag(dropped.clone());
            async move {
                let chunks = futures::stream::repeat_with(move || {
                    let _alive = &flag;
                    Ok::<_, Infallible>(Bytes::from(vec![b'x'; 64 * 1024]))
                });
                Body::from_stream(chunks)
            }
        }),
    )
}

/// Review SEC-6: a client that requests a big response and never reads it
/// held the connection and the response's buffers for as long as it stayed
/// connected (100 such sockets: +208 MB RSS). Now the response is aborted
/// after the write timeout and its body dropped.
#[tokio::test]
async fn a_client_that_stops_reading_is_dropped_after_the_write_timeout() {
    let dropped = Arc::new(AtomicBool::new(false));
    let mut config = local_config();
    config.limits.write_timeout = Some(Duration::from_secs(1));
    let net = FluxNet::start(config, endless(dropped.clone())).await.unwrap();
    // A small receive buffer, as in the reviewer's stall.py.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut stalled = socket.connect(net.local_addr()).await.unwrap();
    stalled
        .write_all(b"GET /endless HTTP/1.1\r\nHost: flux\r\n\r\n")
        .await
        .unwrap();
    let t0 = Instant::now();
    while !dropped.load(Ordering::SeqCst) {
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "a stalled response must be aborted after the write timeout"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("stalled response dropped after {:?}", t0.elapsed());
    drop(stalled);
    net.shutdown(Duration::from_secs(1)).await;
}

/// The write timeout counts time without progress, not response length: a
/// client that reads slower than the server writes, for longer than the
/// write timeout, keeps its stream.
///
/// The reader must still let the server make progress within the timeout.
/// Linux wakes a blocked writer only once about half of its send buffer has
/// drained, and on loopback it autotunes that buffer to megabytes, so a
/// reader behind a 4 KiB receive window (a few KB every 200 ms) leaves the
/// server without progress for longer than 2 s and is rightly dropped. This
/// reader takes at most 64 KiB every 20 ms (about 3 MB/s, far below what the
/// endless body could deliver) for 3x the write timeout.
#[tokio::test]
async fn a_slow_but_reading_client_keeps_its_stream() {
    let dropped = Arc::new(AtomicBool::new(false));
    let mut config = local_config();
    config.limits.write_timeout = Some(Duration::from_secs(2));
    let net = FluxNet::start(config, endless(dropped.clone())).await.unwrap();
    let mut slow = TcpStream::connect(net.local_addr()).await.unwrap();
    slow.write_all(b"GET /endless HTTP/1.1\r\nHost: flux\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let mut got = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(6) {
        let n = tokio::time::timeout(Duration::from_secs(5), slow.read(&mut buf))
            .await
            .expect("data keeps coming")
            .unwrap();
        assert!(n > 0, "the server closed a stream that was being read");
        got += n;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!dropped.load(Ordering::SeqCst), "body dropped after {got} bytes");
    drop(slow);
    net.shutdown(Duration::from_secs(1)).await;
}

/// Pipelined requests on one keep-alive connection are answered in order.
#[tokio::test]
async fn pipelined_requests_are_answered_in_order() {
    let net = FluxNet::start(local_config(), router()).await.unwrap();
    let mut stream = TcpStream::connect(net.local_addr()).await.unwrap();
    let one = "GET /whoami HTTP/1.1\r\nHost: flux\r\n\r\n";
    let last = "GET /whoami HTTP/1.1\r\nHost: flux\r\nConnection: close\r\n\r\n";
    stream
        .write_all(format!("{one}{one}{last}").as_bytes())
        .await
        .unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut raw))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert_eq!(text.matches("HTTP/1.1 200 OK").count(), 3, "{text}");
    net.shutdown(Duration::from_secs(1)).await;
}

/// A stand-in `tailscale` CLI whose `status --json` reports a running node
/// whose only address is 127.0.0.1, so "system mode" binds a loopback port:
/// the tailnet listener's bookkeeping runs without any Tailscale.
#[cfg(unix)]
fn fake_tailscale() -> std::path::PathBuf {
    fake_tailscale_with(None)
}

/// [`fake_tailscale`], and with `cert_pem` the tailnet issues certificates:
/// `status` lists the node's name in `CertDomains` and `cert … <name>`
/// prints `cert_pem`, as `tailscale cert --cert-file - --key-file -` does.
#[cfg(unix)]
fn fake_tailscale_with(cert_pem: Option<&str>) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("flux-net-fake-ts-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cli = dir.join("tailscale");
    let domains = if cert_pem.is_some() {
        r#"["fake.tail0000.ts.net"]"#
    } else {
        "null"
    };
    let status = format!(
        r#"{{"BackendState":"Running","TailscaleIPs":["127.0.0.1"],"Self":{{"DNSName":"fake.tail0000.ts.net.","TailscaleIPs":["127.0.0.1"],"UserID":1}},"User":{{"1":{{"LoginName":"owner@example.com"}}}},"CertDomains":{domains}}}"#
    );
    std::fs::write(dir.join("cert.pem"), cert_pem.unwrap_or_default()).unwrap();
    std::fs::write(
        &cli,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  status) echo '{status}' ;;\n  cert) [ \"$6\" = fake.tail0000.ts.net ] && cat '{pem}' ;;\n  *) echo 'peer not found' >&2; exit 1 ;;\nesac\n",
            pem = dir.join("cert.pem").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    cli
}

/// The running tailnet's info once it satisfies `ready`.
#[cfg(unix)]
async fn running_when(
    net: &FluxNet,
    ready: impl Fn(&flux_net::TailnetInfo) -> bool,
) -> flux_net::TailnetInfo {
    let mut rx = net.tailnet_status();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let TailnetStatus::Running(info) = &*rx.borrow_and_update() {
                if ready(info) {
                    return info.clone();
                }
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("the tailnet listener gets there")
}

/// `GET /whoami` over TLS to `name` at `addr`, trusting only `cert`.
#[cfg(unix)]
async fn https_whoami(
    addr: SocketAddr,
    name: &str,
    cert: &rustls::pki_types::CertificateDer<'static>,
) -> std::io::Result<serde_json::Value> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(flux_net::tls::provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = TcpStream::connect(addr).await?;
    let server_name = rustls::pki_types::ServerName::try_from(name.to_string()).unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
        .connect(server_name, tcp)
        .await?;
    tls.write_all(format!("GET /whoami HTTP/1.1\r\nHost: {name}\r\nConnection: close\r\n\r\n").as_bytes())
        .await?;
    let mut raw = Vec::new();
    match tls.read_to_end(&mut raw).await {
        Err(e) if e.kind() != std::io::ErrorKind::UnexpectedEof => return Err(e),
        _ => {}
    }
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    Ok(serde_json::from_str(body).unwrap())
}

/// The tailnet listener's address once its status says Running.
#[cfg(unix)]
async fn running_at(net: &FluxNet) -> SocketAddr {
    let mut rx = net.tailnet_status();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.borrow_and_update().clone() {
                TailnetStatus::Running(info) => return info.addrs[0],
                TailnetStatus::Failed { error } => panic!("tailnet failed: {error}"),
                TailnetStatus::Unavailable { reason } => panic!("tailnet unavailable: {reason}"),
                _ => {}
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("the tailnet listener starts")
}

/// HTTPS on the tailnet with this machine's Tailscale, end to end: the
/// certificate comes from `tailscale cert`, the tailnet port then speaks
/// HTTPS and plain HTTP, the status gives the `https://` address, and the
/// caller is the same person either way. Switching HTTPS off replaces the
/// listener with one that speaks plain HTTP only.
#[cfg(unix)]
#[tokio::test]
async fn system_mode_https_serves_tailscales_certificate_beside_http() {
    let issued = rcgen::generate_simple_self_signed(vec!["fake.tail0000.ts.net".to_string()]).unwrap();
    let pem = format!("{}{}", issued.cert.pem(), issued.signing_key.serialize_pem());
    let cert = issued.cert.der().clone();
    let cli = fake_tailscale_with(Some(&pem));
    let system = |https| {
        TailnetConfig::System(SystemConfig {
            port: 0,
            cli: Some(cli.clone()),
            https,
            ..Default::default()
        })
    };
    let mut net = FluxNet::start(local_config(), router()).await.unwrap();

    net.set_tailnet(system(true), Duration::from_secs(1)).await;
    let info = running_when(&net, |t| t.tls).await;
    let addr = info.addrs[0];
    assert_eq!(info.tls_error, None);
    let https_url = format!("https://fake.tail0000.ts.net:{}", addr.port());
    assert_eq!(net.tailnet_info().url.as_deref(), Some(https_url.as_str()));
    assert!(net.tailnet_info().https);
    let over_tls = https_whoami(addr, "fake.tail0000.ts.net", &cert).await.unwrap();
    let plain = whoami(addr).await;
    assert_eq!(over_tls, plain, "the same caller over either protocol");
    assert_eq!(
        over_tls["you"]["role"],
        serde_json::to_value(Role::Owner).unwrap()
    );

    net.set_tailnet(system(false), Duration::from_secs(1)).await;
    let addr = running_at(&net).await;
    assert!(https_whoami(addr, "fake.tail0000.ts.net", &cert).await.is_err());
    assert_eq!(whoami(addr).await["path"], "localhost");
    assert!(net.tailnet_info().url.unwrap().starts_with("http://"));
    net.shutdown(Duration::from_secs(2)).await;
    let _ = std::fs::remove_dir_all(cli.parent().unwrap());
}

/// `FluxNet::set_tailnet` replaces only the tailnet listener: the local
/// listener and a stream in flight on it carry on through two switches.
#[cfg(unix)]
#[tokio::test]
async fn set_tailnet_replaces_only_the_tailnet_listener() {
    use flux_core::api::TailnetMode;
    let cli = fake_tailscale();
    let system = || {
        TailnetConfig::System(SystemConfig {
            port: 0,
            cli: Some(cli.clone()),
            ..Default::default()
        })
    };
    let mut net = FluxNet::start(local_config(), router()).await.unwrap();
    let local = net.local_addr();
    assert_eq!(*net.tailnet_status().borrow(), TailnetStatus::Off);
    let download = tokio::spawn(async move {
        let mut stream = TcpStream::connect(local).await.unwrap();
        get_body(&mut stream, "/stream", false).await
    });

    net.set_tailnet(system(), Duration::from_secs(1)).await;
    assert_eq!(net.tailnet_mode(), TailnetMode::System);
    let first = running_at(&net).await;
    assert_ne!(first, local);
    assert_eq!(
        whoami(first).await["path"],
        "localhost",
        "served on the new listener"
    );

    net.set_tailnet(TailnetConfig::Off, Duration::from_secs(1)).await;
    assert_eq!(*net.tailnet_status().borrow(), TailnetStatus::Off);
    assert_eq!(net.tailnet_mode(), TailnetMode::Off);
    assert!(
        TcpStream::connect(first).await.is_err(),
        "the old tailnet listener is closed"
    );
    assert_eq!(
        whoami(local).await["path"],
        "localhost",
        "the local listener never stopped"
    );

    net.set_tailnet(system(), Duration::from_secs(1)).await;
    let second = running_at(&net).await;
    assert_eq!(
        whoami(second).await["you"]["role"],
        serde_json::to_value(Role::Owner).unwrap()
    );

    let (status, body) = download.await.unwrap();
    assert_eq!(
        (status, body.len()),
        (200, CHUNKS * CHUNK),
        "a local stream in flight survives tailnet switches"
    );
    net.shutdown(Duration::from_secs(2)).await;
    assert!(TcpStream::connect(second).await.is_err());
    assert!(TcpStream::connect(local).await.is_err());
    let _ = std::fs::remove_dir_all(cli.parent().unwrap());
}
