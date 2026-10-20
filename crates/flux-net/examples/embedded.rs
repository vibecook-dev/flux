//! Manual test of embedded mode: start flux's own Truffle node and serve a
//! "who am I" page on the tailnet.
//!
//! THIS ADDS A DEVICE TO YOUR TAILNET once you open the printed login URL.
//! Run it yourself; automated tests never do.
//!
//! ```text
//! # $TRUFFLE: a Truffle checkout; $WORK: any scratch directory.
//! cd "$TRUFFLE/packages/sidecar-slim" && \
//!   go build -trimpath -ldflags "-s -w" -o "$WORK/bin/sidecar-slim" .
//! TRUFFLE_SIDECAR_PATH="$WORK/bin/sidecar-slim" \
//!   cargo run -p flux-net --example embedded -- \
//!   --data-dir "$WORK/flux-embedded" --device-name flux-dev [--port 8096] [--tls]
//! ```
//!
//! It prints the login URL (show it as a QR code in a real UI), then the
//! tailnet URL. Open `<url>/whoami` from another tailnet device to see the
//! WhoIs identity and role flux derived. Ctrl-C stops the node; delete the
//! data dir and remove the device in the admin console to undo.

use std::path::PathBuf;
use std::time::Duration;

use axum::routing::get;
use axum::{Extension, Json, Router};
use flux_core::api::Identity;
use flux_net::{ConnInfo, EmbeddedConfig, FluxNet, FluxNetConfig, TailnetConfig, TailnetStatus};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,flux_net=debug")
        .init();
    let mut data_dir = None;
    let mut device_name = "flux-dev".to_string();
    let mut port = 8096u16;
    let mut tls = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => data_dir = args.next().map(PathBuf::from),
            "--device-name" => device_name = args.next().unwrap_or(device_name),
            "--port" => port = args.next().and_then(|p| p.parse().ok()).unwrap_or(port),
            "--tls" => tls = true,
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let data_dir = data_dir.ok_or_else(|| anyhow::anyhow!("--data-dir <dir> is required"))?;

    let router = Router::new().route(
        "/whoami",
        get(|Extension(you): Extension<Option<Identity>>, Extension(conn): Extension<ConnInfo>| async move {
            Json(serde_json::json!({ "you": you, "path": conn.path.as_str(), "server": conn.tailnet_name }))
        }),
    );
    let mut embedded = EmbeddedConfig::new(&data_dir, device_name);
    embedded.port = port;
    embedded.tls = tls;
    let config = FluxNetConfig {
        local_addr: "127.0.0.1:0".parse()?,
        tailnet: TailnetConfig::Embedded(embedded),
        ..Default::default()
    };
    let net = FluxNet::start(config, router).await?;
    println!("local: http://{}/whoami", net.local_addr());

    let mut status = net.tailnet_status();
    loop {
        match status.borrow_and_update().clone() {
            TailnetStatus::NeedsLogin { auth_url } => println!("\nConnect with Tailscale: {auth_url}\n"),
            TailnetStatus::NeedsApproval {
                approve_url: Some(url),
            } => {
                println!("Approve this device in the Tailscale admin console: {url}")
            }
            TailnetStatus::NeedsApproval { approve_url: None } => {
                println!("Approve this device on your control server (e.g. `headscale nodes register`)")
            }
            TailnetStatus::Running(info) => {
                println!(
                    "tailnet: {}/whoami  (owner: {:?})",
                    info.url().unwrap_or_default(),
                    info.owner_login
                )
            }
            TailnetStatus::Failed { error } => {
                eprintln!("failed: {error}");
                break;
            }
            other => println!("{other:?}"),
        }
        tokio::select! {
            changed = status.changed() => if changed.is_err() { break },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    net.shutdown(Duration::from_secs(5)).await;
    Ok(())
}
