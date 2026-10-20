//! Live, read-only test of "use this machine's Tailscale" against the
//! running system tailscaled. It binds a listener on this machine's own
//! Tailscale address, requests it from this machine, and checks the WhoIs
//! identity flux attached. Nothing joins or authenticates a tailnet; the only
//! tailscaled calls are `status --json` and `whois --json`.
//!
//! ```text
//! cargo test -p flux-net --test system_live -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

use axum::routing::get;
use axum::{Extension, Json, Router};
use flux_core::api::{Identity, Role};
use flux_net::system::find_cli;
use flux_net::{
    ConnInfo, FluxNet, FluxNetConfig, SystemConfig, SystemTailscale, TailnetConfig, TailnetStatus,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn get_json(addr: std::net::SocketAddr, path: &str) -> serde_json::Value {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: flux\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    assert!(
        raw.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&raw[..split])
    );
    serde_json::from_slice(&raw[split + 4..]).unwrap()
}

#[tokio::test]
#[ignore = "needs this machine's tailscaled running and logged in; run with --ignored"]
async fn system_mode_identifies_this_machine_via_whois() {
    // What tailscaled itself says, for the expected values.
    let cli = find_cli(None);
    let probe = SystemTailscale::new(cli.clone(), Duration::from_secs(60));
    let status = probe.status().await.expect("tailscale status --json");
    assert!(status.is_running(), "tailscaled is {}", status.backend_state);
    let owner = status.owner_login.clone().expect("this node has an owner login");
    eprintln!(
        "cli={} dns={:?} ips={:?}",
        cli.display(),
        status.dns_name,
        status.self_ips
    );

    let router = Router::new().route(
        "/whoami",
        get(|Extension(you): Extension<Option<Identity>>, Extension(conn): Extension<ConnInfo>| async move {
            Json(serde_json::json!({ "you": you, "path": conn.path.as_str(), "server": conn.tailnet_name }))
        }),
    );
    let config = FluxNetConfig {
        local_addr: "127.0.0.1:0".parse().unwrap(),
        tailnet: TailnetConfig::System(SystemConfig {
            port: 0,
            ..Default::default()
        }),
        ..Default::default()
    };
    let net = FluxNet::start(config, router).await.unwrap();
    let mut rx = net.tailnet_status();
    let info = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let TailnetStatus::Running(info) = rx.borrow_and_update().clone() {
                return info;
            }
            if let TailnetStatus::Failed { error } = &*rx.borrow() {
                panic!("system mode failed: {error}");
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("system mode came up");
    eprintln!("serving on {:?} as {:?}", info.addrs, info.url());
    assert_eq!(
        info.owner_login.as_deref(),
        Some(owner.as_str()),
        "owner learned from tailscaled"
    );

    let addr = *info
        .addrs
        .iter()
        .find(|a| a.is_ipv4())
        .expect("an IPv4 Tailscale address");
    let cold = Instant::now();
    let v = get_json(addr, "/whoami").await;
    let cold = cold.elapsed();
    let warm = Instant::now();
    let v2 = get_json(addr, "/whoami").await;
    let warm = warm.elapsed();
    eprintln!("whoami (whois cold) {cold:?}, (cached) {warm:?}: {v}");

    assert_eq!(
        v["you"]["login_name"],
        owner.as_str(),
        "WhoIs identity of this machine"
    );
    assert_eq!(v["you"]["role"], serde_json::to_value(Role::Owner).unwrap());
    assert_eq!(v["path"], "tailnet-direct");
    assert_eq!(v["server"], serde_json::to_value(&status.dns_name).unwrap());
    assert_eq!(v, v2);

    // The local listener is still the plain localhost path.
    let local = get_json(net.local_addr(), "/whoami").await;
    assert_eq!(local["path"], "localhost");

    net.shutdown(Duration::from_secs(2)).await;
}
