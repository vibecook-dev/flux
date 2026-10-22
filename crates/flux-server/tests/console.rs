//! Console control events on a running flux-server.exe (Windows only). A
//! second event during a graceful stop used to end flux at once (exit
//! 0xC000013A, no "flux stopped"): flux dropped its listeners once the first
//! event arrived, so Windows' default handler took the second.

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Set for the helper process of [`send_ctrl_break`]: the pid to send to.
const SEND_TO: &str = "FLUX_TEST_CTRL_BREAK_TO";

/// The stop a Ctrl-Break starts gives a request in flight 5 s. A second
/// Ctrl-Break 1 s into it is logged and ignored, and flux still ends with
/// "flux stopped" and exit 0.
#[test]
fn a_second_ctrl_break_does_not_cut_the_stop_short() {
    let data = tempfile::tempdir().unwrap();
    // Its own (hidden) console and process group, so the events go to flux
    // only, never to this test or cargo.
    let mut flux = Flux::start(
        flux_core::tool::command(env!("CARGO_BIN_EXE_flux-server"))
            .arg("--data-dir")
            .arg(data.path())
            .args(["--port", "0", "--tailnet", "off", "--log", "info"])
            .args(["--no-scan", "--no-jobs", "--no-watch"])
            .env("NO_COLOR", "1")
            .env_remove("FLUX_CONFIG")
            .env_remove("FLUX_LIBRARIES"),
    );
    let ready = flux.wait_for("flux is ready", Duration::from_secs(60));
    let addr: SocketAddr = ready
        .split_once("url=http://")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|a| a.parse().ok())
        .unwrap_or_else(|| panic!("no address in {ready:?}"));

    // A request in flight: its headers are sent, its body isn't.
    let mut held = TcpStream::connect(addr).unwrap();
    write!(
        held,
        "POST /api/v1/libraries HTTP/1.1\r\nHost: {addr}\r\nX-Flux-Request: 1\r\n\
         Content-Type: application/json\r\nContent-Length: 64\r\n\r\n{{"
    )
    .unwrap();
    held.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));

    send_ctrl_break(flux.pid());
    flux.wait_for("shutting down", Duration::from_secs(10));
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        flux.exit_status(Duration::ZERO).is_none(),
        "the held request should keep the stop going for 5 s:\n{}",
        flux.log()
    );

    send_ctrl_break(flux.pid());
    flux.wait_for("already stopping", Duration::from_secs(10));
    let status = flux.exit_status(Duration::from_secs(30));
    flux.wait_for("flux stopped", Duration::from_secs(5));
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "{status:?}\n{}",
        flux.log()
    );
    drop(held);
}

/// Sends Ctrl-Break to the process group led by `pid`, which has a console
/// of its own. Only a process on that console can, so this runs the test
/// binary again as a helper ([`ctrl_break_sender`]) that joins it.
fn send_ctrl_break(pid: u32) {
    let out = flux_core::tool::command(std::env::current_exe().unwrap())
        .args(["ctrl_break_sender", "--exact", "--nocapture"])
        .env(SEND_TO, pid.to_string())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sender: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The helper process of [`send_ctrl_break`]; without `SEND_TO` it does
/// nothing.
#[test]
#[allow(unsafe_code)]
fn ctrl_break_sender() {
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT,
    };
    let Some(pid) = std::env::var(SEND_TO).ok().and_then(|p| p.parse::<u32>().ok()) else {
        return;
    };
    // SAFETY: calls without pointers. This process leaves its own console
    // (its stdio are pipes), joins flux's, sends to flux's process group
    // only, and leaves again before flux's console can close.
    unsafe {
        FreeConsole();
        let attached = AttachConsole(pid);
        let attach_error = std::io::Error::last_os_error();
        let sent = attached != 0 && GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0;
        let send_error = std::io::Error::last_os_error();
        FreeConsole();
        assert_ne!(attached, 0, "attaching to {pid}'s console: {attach_error}");
        assert!(sent, "sending Ctrl-Break to {pid}: {send_error}");
    }
}

/// flux-server.exe with its log (stderr) read line by line; killed if the
/// test fails before it exits.
struct Flux {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Flux {
    fn start(cmd: &mut std::process::Command) -> Flux {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("flux-server starts");
        let stderr = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Flux {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The first log line (seen before or arriving within `limit`) that
    /// contains `needle`.
    fn wait_for(&mut self, needle: &str, limit: Duration) -> String {
        if let Some(l) = self.seen.iter().find(|l| l.contains(needle)) {
            return l.clone();
        }
        let t0 = Instant::now();
        let ended = loop {
            let left = limit.saturating_sub(t0.elapsed());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(needle) {
                        return line;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break false,
                Err(mpsc::RecvTimeoutError::Disconnected) => break true,
            }
        };
        // A log that ended means flux did (0xC000013A was the bug here).
        let status = self.exit_status(if ended {
            Duration::from_secs(5)
        } else {
            Duration::ZERO
        });
        panic!(
            "no log line with {needle:?} ({}); exit status {status:?}:\n{}",
            if ended {
                "the log ended".to_string()
            } else {
                format!("after {limit:?}")
            },
            self.log()
        );
    }

    /// The exit status, if flux ends within `limit`.
    fn exit_status(&mut self, limit: Duration) -> Option<ExitStatus> {
        let t0 = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if t0.elapsed() >= limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Every log line so far.
    fn log(&mut self) -> String {
        self.seen.extend(self.lines.try_iter());
        self.seen.join("\n")
    }
}

impl Drop for Flux {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
