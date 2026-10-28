//! The updater end to end, with real processes: flux-server installed from a
//! release-style package in a scratch folder, a stand-in for GitHub's
//! releases API serving a "newer" package (this same binary, which
//! `FLUX_UPDATE_CURRENT` makes the running flux take for an older one), and
//! the update through the API: download, SHA256SUMS, unpack, try, swap
//! (renaming the running binary, which on Windows is the delicate part), and
//! the restart. Then `flux-server update --check` and `--rollback` from a
//! terminal.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, State};
use axum::routing::get;
use axum::Router;
use flux_core::api::{UpdateInfo, UpdatePhase};
use flux_server::update::install::{Layout, MARKER, PREVIOUS_DIR};
use flux_server::update::TARGET;
use sha2::{Digest, Sha256};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const BIN: &str = env!("CARGO_BIN_EXE_flux-server");
/// A pre-release, so it follows pre-releases (this version is one until 0.1.0).
const OLDER: &str = "0.0.1-alpha.1";

fn layout() -> Layout {
    Layout::native()
}

/// A package of this binary, as a release has it: `(file name, bytes)`.
fn package() -> (String, Vec<u8>) {
    let top = format!("flux-{VERSION}-{TARGET}");
    let exe = std::fs::read(BIN).unwrap();
    match layout() {
        Layout::Unix => {
            let mut tar = tar::Builder::new(Vec::new());
            for (rel, body, mode) in [
                ("bin/flux-server".to_string(), exe, 0o755),
                (format!("share/flux/{MARKER}"), b"archive\n".to_vec(), 0o644),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(mode);
                h.set_cksum();
                tar.append_data(&mut h, format!("{top}/{rel}"), body.as_slice())
                    .unwrap();
            }
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(&tar.into_inner().unwrap()).unwrap();
            (format!("{top}.tar.gz"), gz.finish().unwrap())
        }
        Layout::Flat => (
            format!("{top}.zip"),
            stored_zip(&[
                (format!("{top}/"), Vec::new()),
                (format!("{top}/flux-server.exe"), exe),
                (format!("{top}/{MARKER}"), b"archive\n".to_vec()),
            ]),
        ),
    }
}

/// A zip with stored entries (what the updater reads, like zip.py's folders).
fn stored_zip(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, data) in files {
        let crc = crc32fast::hash(data);
        let offset = out.len() as u32;
        let head = |sig: u32, central_dir: bool| {
            let mut h = Vec::new();
            h.extend(sig.to_le_bytes());
            if central_dir {
                h.extend([20, 3]);
            }
            h.extend([20, 0, 0, 0, 0, 0, 0, 0, 0x21, 0]);
            h.extend(crc.to_le_bytes());
            h.extend((data.len() as u32).to_le_bytes());
            h.extend((data.len() as u32).to_le_bytes());
            h.extend((name.len() as u16).to_le_bytes());
            h.extend([0, 0]);
            if central_dir {
                h.extend([0u8; 10]);
                h.extend(offset.to_le_bytes());
            }
            h.extend(name.as_bytes());
            h
        };
        out.extend(head(0x0403_4b50, false));
        out.extend(data);
        central.extend(head(0x0201_4b50, true));
    }
    let cd_offset = out.len() as u32;
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0, 0, 0, 0]);
    out.extend((files.len() as u16).to_le_bytes());
    out.extend((files.len() as u16).to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend([0, 0]);
    out
}

#[derive(Clone)]
struct Fake {
    releases: String,
    files: std::sync::Arc<Vec<(String, Vec<u8>)>>,
}

/// A stand-in for GitHub: `/releases` and `/download/{name}`.
async fn fake_github(sums_ok: bool) -> String {
    let (name, bytes) = package();
    let sha = if sums_ok {
        hex(&Sha256::digest(&bytes))
    } else {
        "0".repeat(64)
    };
    let sums = format!("{sha}  {name}\n").into_bytes();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let releases = serde_json::json!([{
        "tag_name": format!("v{VERSION}"),
        "draft": false,
        "prerelease": VERSION.contains('-'),
        "html_url": format!("{base}/release"),
        "published_at": "2026-10-28T00:00:00Z",
        "body": "### Features\n\n* **update:** the test release\n",
        "assets": [
            {"name": name, "browser_download_url": format!("{base}/download/{name}"), "size": bytes.len()},
            {"name": "SHA256SUMS", "browser_download_url": format!("{base}/download/SHA256SUMS"), "size": sums.len()},
        ],
    }])
    .to_string();
    let fake = Fake {
        releases,
        files: std::sync::Arc::new(vec![(name, bytes), ("SHA256SUMS".into(), sums)]),
    };
    let app = Router::new()
        .route(
            "/releases",
            get(|State(f): State<Fake>| async move { f.releases.clone() }),
        )
        .route(
            "/download/{name}",
            get(
                |State(f): State<Fake>, UrlPath(name): UrlPath<String>| async move {
                    f.files
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map(|(_, b)| b.clone())
                        .unwrap_or_default()
                },
            ),
        )
        .with_state(fake);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("{base}/releases")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A scratch install of this binary as a release package lays it out.
fn install(dir: &Path) -> PathBuf {
    let root = dir.join("flux");
    let exe = root.join(layout().exe());
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::copy(BIN, &exe).unwrap();
    let marker = match layout() {
        Layout::Unix => root.join("share/flux").join(MARKER),
        Layout::Flat => root.join(MARKER),
    };
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(marker, "archive\n").unwrap();
    // Not flux's: an update must leave it.
    std::fs::write(root.join("notes.txt"), "mine").unwrap();
    root
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The server process; killed (with whatever flux started in its place) when dropped.
struct Running {
    child: Child,
    data: PathBuf,
    port: u16,
}

impl Running {
    fn start(root: &Path, data: &Path, source: &str) -> Running {
        let port = free_port();
        let log = std::fs::File::create(data.with_extension("log")).unwrap();
        let child = Command::new(root.join(layout().exe()))
            .args([
                "--data-dir",
                data.to_str().unwrap(),
                "--port",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
            ])
            .args([
                "--tailnet",
                "off",
                "--no-scan",
                "--no-jobs",
                "--no-watch",
                "--update-check",
                "false",
            ])
            .env("FLUX_UPDATE_SOURCE", source)
            .env("FLUX_UPDATE_CURRENT", OLDER)
            .env("FLUX_RESTART", "spawn")
            .env("FLUX_LOG", "info")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        Running {
            child,
            data: data.to_path_buf(),
            port,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// The pid in the data dir's server.json (the restarted flux's on Windows).
    fn pid(&self) -> Option<u32> {
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.data.join("server.json")).ok()?).ok()?;
        v["pid"].as_u64().map(|p| p as u32)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.data.with_extension("log")).unwrap_or_default()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(pid) = self.pid().filter(|p| *p != self.child.id()) {
            kill(pid);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn kill(pid: u32) {
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output();
    #[cfg(unix)]
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
}

async fn wait_for<T>(what: &str, server: &Running, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "{what}: timed out. Server log:\n{}",
            server.log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Until nothing answers on `port` any more (a killed process can take a moment on Windows).
async fn gone(port: u16) {
    let start = Instant::now();
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "flux still answers on {port}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn update_info(http: &reqwest::Client, server: &Running) -> Option<UpdateInfo> {
    http.get(server.url("/api/v1/admin/update"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_itself_restarts_and_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let source = fake_github(true).await;
    let root = install(dir.path());
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let server = Running::start(&root, &data, &source);
    let http = reqwest::Client::new();

    let before = wait_for("the server starts", &server, async || {
        update_info(&http, &server).await
    })
    .await;
    assert_eq!(before.current, OLDER);
    assert!(before.install.can_apply, "{:?}", before.install);
    let first_pid = server.pid();

    let checked: UpdateInfo = http
        .post(server.url("/api/v1/admin/update/check"))
        .header("x-flux-request", "1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let available = checked.available.expect("the check finds the newer release");
    assert_eq!(available.version, VERSION);
    assert_eq!(available.highlights, ["update: the test release"]);

    let res = http
        .post(server.url("/api/v1/admin/update/apply"))
        .header("x-flux-request", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);

    // The restarted flux (a new process image: idle again) knows what it replaced.
    let after = wait_for("flux restarts as the new version", &server, async || {
        update_info(&http, &server)
            .await
            .filter(|i| i.phase == UpdatePhase::Idle && i.previous.as_deref() == Some(OLDER))
    })
    .await;
    assert_eq!(after.error, None);
    if cfg!(unix) {
        assert_eq!(server.pid(), first_pid, "in place: the same process");
    } else {
        assert_ne!(server.pid(), first_pid, "a new process");
    }
    assert!(root.join(layout().exe()).is_file());
    assert!(
        root.join(PREVIOUS_DIR).join(layout().exe()).is_file(),
        "the replaced version is kept"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        "mine",
        "not flux's: left alone"
    );
    assert!(!root.join(".flux-update").exists(), "nothing left unpacked");
    assert!(!data.join("updates").exists(), "the download is gone");

    // --rollback refuses while flux runs, then puts the kept version back.
    let port = server.port;
    let cli = |args: &[&str]| {
        Command::new(root.join(layout().exe()))
            .arg("--data-dir")
            .arg(&data)
            .args(["--port", &port.to_string(), "update"])
            .args(args)
            .env("FLUX_UPDATE_SOURCE", &source)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let refused = cli(&["--rollback", "--yes"]);
    assert_eq!(
        refused.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("stop it first"));
    drop(server);
    gone(port).await;
    let back = cli(&["--rollback", "--yes"]);
    assert_eq!(
        back.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&back.stdout),
        String::from_utf8_lossy(&back.stderr)
    );
    assert!(String::from_utf8_lossy(&back.stdout).contains(&format!("flux {OLDER} is back")));
    assert!(
        root.join(PREVIOUS_DIR).join(layout().exe()).is_file(),
        "and the other one is kept in turn"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_that_doesnt_match_sha256sums_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let source = fake_github(false).await;
    let root = install(dir.path());
    let exe_before = std::fs::read(root.join(layout().exe())).unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let server = Running::start(&root, &data, &source);
    let http = reqwest::Client::new();
    wait_for("the server starts", &server, async || {
        update_info(&http, &server).await
    })
    .await;
    let res = http
        .post(server.url("/api/v1/admin/update/apply"))
        .header("x-flux-request", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);
    let failed = wait_for("the update fails", &server, async || {
        update_info(&http, &server)
            .await
            .filter(|i| i.phase == UpdatePhase::Failed)
    })
    .await;
    assert!(
        failed.error.as_deref().unwrap_or_default().contains("SHA256SUMS"),
        "{:?}",
        failed.error
    );
    assert_eq!(
        std::fs::read(root.join(layout().exe())).unwrap(),
        exe_before,
        "the install is as it was"
    );
    assert!(!root.join(PREVIOUS_DIR).exists());
    // And it still serves.
    assert!(http
        .get(server.url("/api/v1/server"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

#[tokio::test(flavor = "multi_thread")]
async fn update_check_from_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let source = fake_github(true).await;
    let root = install(dir.path());
    let data = dir.path().join("data");
    let run = move |current: &str| {
        Command::new(root.join(layout().exe()))
            .arg("--data-dir")
            .arg(&data)
            .args(["update", "--check"])
            .env("FLUX_UPDATE_SOURCE", &source)
            .env("FLUX_UPDATE_CURRENT", current)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let out = tokio::task::spawn_blocking(move || (run(OLDER), run(VERSION)))
        .await
        .unwrap();
    let (older, current) = out;
    assert_eq!(
        older.status.code(),
        Some(10),
        "{}",
        String::from_utf8_lossy(&older.stderr)
    );
    let said = String::from_utf8_lossy(&older.stdout);
    assert!(
        said.contains(&format!("flux {VERSION} is out (this is {OLDER})")),
        "{said}"
    );
    assert!(said.contains("the test release"), "{said}");
    assert_eq!(current.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&current.stdout).contains("is up to date"));
}
