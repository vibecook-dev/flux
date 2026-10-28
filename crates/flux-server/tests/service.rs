//! flux as a service of the machine, for real: `flux-server service install`
//! (a LaunchAgent, a systemd user unit or a Windows service, under a test
//! name), then flux answers, comes back after asking to restart (as after an
//! update), stops, starts, and is uninstalled.
//!
//! It installs a service, so it runs only where `FLUX_TEST_SERVICES=1` says
//! that's fine (CI's machines, which ci.yml prepares); anywhere else it says
//! so and passes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime};

fn flux(name: &str, data: &Path, port: u16, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flux-server"))
        .args(["--service-name", name, "--data-dir"])
        .arg(data)
        .args(["--port", &port.to_string()])
        .args(args)
        .env_remove("FLUX_CONFIG")
        .output()
        .expect("flux-server runs")
}

fn said(o: &Output) -> String {
    format!(
        "status {:?}\n{}{}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// Uninstalls the test's service however the test ends.
struct Installed {
    name: String,
    data: PathBuf,
    port: u16,
}

impl Drop for Installed {
    fn drop(&mut self) {
        let _ = flux(&self.name, &self.data, self.port, &["service", "uninstall"]);
    }
}

async fn answers(port: u16) -> bool {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/api/v1/server"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// When `server.json` (which a starting flux writes) was last written.
fn started_at(data: &Path) -> Option<SystemTime> {
    std::fs::metadata(data.join("server.json"))
        .and_then(|m| m.modified())
        .ok()
}

async fn until(what: &str, within: Duration, data: &Path, mut done: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !done().await {
        if start.elapsed() > within {
            let log = ["flux.log", "../flux.log"]
                .iter()
                .find_map(|f| std::fs::read_to_string(data.join(f)).ok())
                .unwrap_or_default();
            panic!("{what}: not within {within:?}. flux's log (Windows):\n{log}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn flux_runs_as_a_service_of_this_machine() {
    if std::env::var_os("FLUX_TEST_SERVICES").is_none() {
        eprintln!("skipped: this installs a service (set FLUX_TEST_SERVICES=1 where that's fine)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let port = free_port();
    let name = format!("flux-test-{}", std::process::id());

    let installed = flux(&name, &data, port, &["service", "install"]);
    assert!(installed.status.success(), "install: {}", said(&installed));
    let _uninstall = Installed {
        name: name.clone(),
        data: data.clone(),
        port,
    };
    until("flux answers", Duration::from_secs(90), &data, async || {
        answers(port).await
    })
    .await;
    let status = flux(&name, &data, port, &["service", "status"]);
    assert!(said(&status).contains("running"), "status: {}", said(&status));

    // Asking to restart, as after an update: in its own place on Unix, and on
    // Windows by exiting for the service manager to start flux again.
    let before = started_at(&data);
    let restart = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/api/v1/admin/restart"))
        .header("x-flux-request", "1")
        .send()
        .await
        .expect("the restart request");
    assert_eq!(restart.status().as_u16(), 202, "{:?}", restart.text().await);
    until(
        "flux comes back after its restart",
        Duration::from_secs(120),
        &data,
        async || {
            let again = started_at(&data);
            again.is_some() && again != before && answers(port).await
        },
    )
    .await;

    // Stopped, it stays stopped; started, it's back.
    let stop = flux(&name, &data, port, &["service", "stop"]);
    assert!(stop.status.success(), "stop: {}", said(&stop));
    until("flux stops", Duration::from_secs(60), &data, async || {
        !answers(port).await
    })
    .await;
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert!(!answers(port).await, "a stop isn't undone by the service manager");
    let start = flux(&name, &data, port, &["service", "start"]);
    assert!(start.status.success(), "start: {}", said(&start));
    until(
        "flux answers after a start",
        Duration::from_secs(90),
        &data,
        async || answers(port).await,
    )
    .await;

    let removed = flux(&name, &data, port, &["service", "uninstall"]);
    assert!(removed.status.success(), "uninstall: {}", said(&removed));
    until("flux is gone", Duration::from_secs(60), &data, async || {
        !answers(port).await
    })
    .await;
    let status = flux(&name, &data, port, &["service", "status"]);
    assert!(
        said(&status).contains("isn't installed"),
        "status: {}",
        said(&status)
    );
}
