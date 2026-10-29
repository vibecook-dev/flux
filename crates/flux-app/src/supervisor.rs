//! flux-server, kept running while the app is open.
//!
//! A thread starts flux-server with `FLUX_RESTART=exit`, its output appended
//! to the log, and waits for it: it is up once `server.json` names its
//! process and its address answers. When it stops on its own it is started
//! again, after a second, then two, and so on; five stops within two minutes
//! and it stays stopped until asked to start. Exit status 75 is flux asking
//! to start again after an update: the app starts itself again, as the new
//! version, which starts the new flux-server.
//!
//! A flux already running for this person (from `flux-server service
//! install`, Homebrew's service or a terminal) is left alone: the app shows
//! it and starts its own once that one has stopped. A flux-server of this
//! very bundle that outlived an app that was killed is stopped first.

use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::probe;

/// flux-server's exit status when it asks to be started again
/// (`EX_TEMPFAIL`; flux-server's `main.rs`).
pub const RESTART: i32 = 75;
/// Stops within this window count towards [`GIVE_UP_AFTER`].
const CRASH_WINDOW: Duration = Duration::from_secs(120);
const GIVE_UP_AFTER: usize = 5;
/// How long a stop may take before flux-server is killed.
pub const STOP_WITHIN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Starting,
    Running {
        version: String,
        url: String,
    },
    /// Another flux answers here; the app didn't start its own.
    Elsewhere {
        url: String,
    },
    /// Stopped, and not starting again until asked.
    Stopped {
        why: String,
    },
    /// flux asked to start again (an update): the app starts itself again.
    Relaunch,
    /// The app is quitting and flux has stopped.
    Quit,
}

/// What to run, and where.
#[derive(Debug, Clone)]
pub struct Config {
    pub server: PathBuf,
    pub data_dir: PathBuf,
    pub log: PathBuf,
    /// Where to look for a flux that runs already ([`probe::DEFAULT_URL`]).
    pub default_url: String,
}

pub struct Supervisor {
    cfg: Config,
    state: Mutex<State>,
    /// The running flux-server's process, 0 when none.
    pid: AtomicI32,
    quitting: AtomicBool,
    start_asked: AtomicBool,
    restart_asked: AtomicBool,
}

impl Supervisor {
    pub fn new(cfg: Config) -> Arc<Supervisor> {
        Arc::new(Supervisor {
            cfg,
            state: Mutex::new(State::Starting),
            pid: AtomicI32::new(0),
            quitting: AtomicBool::new(false),
            start_asked: AtomicBool::new(false),
            restart_asked: AtomicBool::new(false),
        })
    }

    pub fn spawn(self: &Arc<Self>) -> std::thread::JoinHandle<()> {
        let me = self.clone();
        std::thread::Builder::new()
            .name("flux-server".into())
            .spawn(move || me.run())
            .expect("starting the supervisor thread")
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn set(&self, s: State) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = s;
    }

    /// Starts flux again after it was left stopped.
    pub fn ask_start(&self) {
        self.start_asked.store(true, Ordering::SeqCst);
    }

    /// Stops flux-server and starts it again (after a change to its
    /// settings), or starts it if it is stopped.
    pub fn restart(&self) {
        let pid = self.pid.load(Ordering::SeqCst);
        if pid > 0 {
            self.restart_asked.store(true, Ordering::SeqCst);
            signal(pid, libc::SIGTERM);
        } else {
            self.ask_start();
        }
    }

    /// Stops flux-server (SIGTERM, then SIGKILL after `within`) and waits for
    /// it; the thread then ends. True when it stopped by itself.
    pub fn stop(&self, within: Duration) -> bool {
        self.quitting.store(true, Ordering::SeqCst);
        let pid = self.pid.load(Ordering::SeqCst);
        if pid > 0 {
            signal(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + within;
        while self.pid.load(Ordering::SeqCst) > 0 {
            if Instant::now() > deadline {
                let pid = self.pid.load(Ordering::SeqCst);
                if pid > 0 {
                    signal(pid, libc::SIGKILL);
                }
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    fn quitting(&self) -> bool {
        self.quitting.load(Ordering::SeqCst)
    }

    /// Sleeps `d`, or less if the app quits: false when it does.
    fn pause(&self, d: Duration) -> bool {
        let until = Instant::now() + d;
        while Instant::now() < until {
            if self.quitting() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        !self.quitting()
    }

    fn run(&self) {
        let mut stops: Vec<Instant> = Vec::new();
        'serve: loop {
            if self.quitting() {
                break;
            }
            // Someone else's flux: show it, and take over once it stops.
            while let Some(url) = self.elsewhere() {
                self.set(State::Elsewhere { url });
                if !self.pause(Duration::from_secs(5)) {
                    break 'serve;
                }
            }
            self.set(State::Starting);
            let mut child = match self.start() {
                Ok(c) => c,
                Err(e) => {
                    self.set(State::Stopped {
                        why: format!("flux-server didn't start: {e}"),
                    });
                    if !self.wait_for_start() {
                        break;
                    }
                    continue;
                }
            };
            let status = self.watch(&mut child);
            self.pid.store(0, Ordering::SeqCst);
            if self.quitting() {
                break;
            }
            if status.code() == Some(RESTART) {
                self.set(State::Relaunch);
                return;
            }
            if self.restart_asked.swap(false, Ordering::SeqCst) {
                continue;
            }
            let now = Instant::now();
            stops.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
            stops.push(now);
            let why = describe(&status);
            if stops.len() >= GIVE_UP_AFTER {
                self.set(State::Stopped {
                    why: format!("{why}; see the log"),
                });
                if !self.wait_for_start() {
                    break;
                }
                stops.clear();
                continue;
            }
            self.set(State::Starting);
            if !self.pause(Duration::from_secs(stops.len() as u64)) {
                break;
            }
        }
        self.set(State::Quit);
    }

    /// Waits for [`ask_start`](Self::ask_start): false when the app quits.
    fn wait_for_start(&self) -> bool {
        self.start_asked.store(false, Ordering::SeqCst);
        while !self.start_asked.swap(false, Ordering::SeqCst) {
            if !self.pause(Duration::from_millis(250)) {
                return false;
            }
        }
        true
    }

    /// The address of a flux this app didn't start, if one answers: the one
    /// in `server.json` while its process lives, or the default address. One
    /// left behind by this bundle (the app was killed) is stopped instead.
    fn elsewhere(&self) -> Option<String> {
        let ours = self.pid.load(Ordering::SeqCst);
        if let Some(f) = probe::read_server_file(&self.cfg.data_dir) {
            if let Some(pid) = f.pid.and_then(|p| i32::try_from(p).ok()) {
                if pid != ours && alive(pid) {
                    if runs(pid, &self.cfg.server) {
                        stop_orphan(pid);
                    } else if probe::version_at(&f.url, Duration::from_secs(2)).is_some() {
                        return Some(f.url);
                    }
                }
            }
        }
        probe::version_at(&self.cfg.default_url, Duration::from_secs(2)).map(|_| self.cfg.default_url.clone())
    }

    fn start(&self) -> std::io::Result<Child> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.cfg.log)?;
        let child = Command::new(&self.cfg.server)
            .env("FLUX_RESTART", "exit")
            .env("NO_COLOR", "1")
            .current_dir(self.cfg.data_dir.parent().unwrap_or(&self.cfg.data_dir))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        self.pid.store(child.id() as i32, Ordering::SeqCst);
        Ok(child)
    }

    /// Until `child` exits: running once it answers.
    fn watch(&self, child: &mut Child) -> ExitStatus {
        let mut up = false;
        let mut asked = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) => {}
                Err(_) => {
                    if let Ok(status) = child.wait() {
                        return status;
                    }
                }
            }
            if !up && asked.elapsed() >= Duration::from_millis(500) {
                asked = Instant::now();
                if let Some(f) =
                    probe::read_server_file(&self.cfg.data_dir).filter(|f| f.pid == Some(child.id()))
                {
                    if let Some(version) = probe::version_at(&f.url, Duration::from_secs(2)) {
                        up = true;
                        self.set(State::Running { version, url: f.url });
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Why a flux-server stopped, for a person.
pub fn describe(status: &ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("flux stopped (exit status {code})"),
        (None, Some(sig)) => format!("flux stopped (signal {sig})"),
        _ => "flux stopped".to_string(),
    }
}

fn signal(pid: i32, sig: i32) {
    // SAFETY: kill(2) on a process this app started (or found running its
    // own flux-server); no memory is involved.
    #[allow(unsafe_code)]
    unsafe {
        libc::kill(pid, sig);
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    #[allow(unsafe_code)]
    let r = unsafe { libc::kill(pid, 0) };
    r == 0
}

/// Whether process `pid` runs the program at `exe`.
fn runs(pid: i32, exe: &std::path::Path) -> bool {
    let Some(path) = exe_of(pid) else {
        return false;
    };
    let real = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    real(&path) == real(exe)
}

#[cfg(target_os = "macos")]
fn exe_of(pid: i32) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: proc_pidpath writes at most `buf.len()` bytes into `buf`.
    #[allow(unsafe_code)]
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(PathBuf::from(String::from_utf8(buf).ok()?))
}

#[cfg(not(target_os = "macos"))]
fn exe_of(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Stops a flux-server of this bundle that an earlier app left running.
fn stop_orphan(pid: i32) {
    signal(pid, libc::SIGTERM);
    let deadline = Instant::now() + STOP_WITHIN;
    while alive(pid) {
        if Instant::now() > deadline {
            signal(pid, libc::SIGKILL);
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in for flux-server: a shell script.
    fn script(dir: &std::path::Path, body: &str) -> PathBuf {
        let p = dir.join("flux-server");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn config(dir: &std::path::Path, server: PathBuf) -> Config {
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        Config {
            server,
            data_dir,
            log: dir.join("flux.log"),
            // Nothing listens on port 9 (discard) here.
            default_url: "http://127.0.0.1:9".into(),
        }
    }

    fn wait_for(sup: &Supervisor, what: impl Fn(&State) -> bool) -> State {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let s = sup.state();
            if what(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "still {s:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn an_update_ends_in_a_relaunch() {
        let d = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(config(d.path(), script(d.path(), "echo restarting; exit 75")));
        sup.spawn().join().unwrap();
        assert_eq!(sup.state(), State::Relaunch);
        let log = std::fs::read_to_string(d.path().join("flux.log")).unwrap();
        assert_eq!(log, "restarting\n");
    }

    #[test]
    fn a_flux_that_keeps_stopping_is_left_stopped_until_asked() {
        let d = tempfile::tempdir().unwrap();
        let count = d.path().join("count");
        let body = format!("echo x >> '{}'; exit 3", count.display());
        let sup = Supervisor::new(config(d.path(), script(d.path(), &body)));
        let t = sup.spawn();
        // Waits of 1 to 4 s between the five tries.
        let s = wait_for(&sup, |s| matches!(s, State::Stopped { .. }));
        assert_eq!(
            s,
            State::Stopped {
                why: "flux stopped (exit status 3); see the log".into()
            }
        );
        let runs = || std::fs::read_to_string(&count).unwrap().lines().count();
        assert_eq!(runs(), GIVE_UP_AFTER);
        sup.ask_start();
        wait_for(&sup, |_| runs() > GIVE_UP_AFTER);
        assert!(sup.stop(Duration::from_secs(5)));
        t.join().unwrap();
        assert_eq!(sup.state(), State::Quit);
    }

    #[test]
    fn quitting_stops_flux_server() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("data");
        // Writes server.json for its own process, then waits to be stopped.
        let body = format!(
            "echo \"{{\\\"url\\\":\\\"http://127.0.0.1:9\\\",\\\"pid\\\":$$}}\" > '{}/server.json'\n\
             trap 'echo stopping; exit 0' TERM\nwhile :; do sleep 0.1; done",
            data.display()
        );
        let sup = Supervisor::new(config(d.path(), script(d.path(), &body)));
        let t = sup.spawn();
        wait_for(&sup, |_| {
            sup.pid.load(Ordering::SeqCst) > 0 && data.join("server.json").exists()
        });
        // It never answers on port 9, so it stays "starting".
        assert_eq!(sup.state(), State::Starting);
        assert!(sup.stop(Duration::from_secs(10)));
        t.join().unwrap();
        assert_eq!(sup.state(), State::Quit);
        assert!(std::fs::read_to_string(d.path().join("flux.log"))
            .unwrap()
            .contains("stopping"));
    }

    #[test]
    fn a_restart_starts_it_again_at_once() {
        let d = tempfile::tempdir().unwrap();
        let count = d.path().join("count");
        let body = format!(
            "echo x >> '{}'\ntrap 'exit 0' TERM\nwhile :; do sleep 0.1; done",
            count.display()
        );
        let sup = Supervisor::new(config(d.path(), script(d.path(), &body)));
        let t = sup.spawn();
        let runs = || {
            std::fs::read_to_string(&count)
                .map(|c| c.lines().count())
                .unwrap_or(0)
        };
        wait_for(&sup, |_| runs() == 1 && sup.pid.load(Ordering::SeqCst) > 0);
        let first = sup.pid.load(Ordering::SeqCst);
        sup.restart();
        wait_for(&sup, |_| runs() == 2 && sup.pid.load(Ordering::SeqCst) > 0);
        assert_ne!(sup.pid.load(Ordering::SeqCst), first);
        assert_eq!(sup.state(), State::Starting, "not counted as a stop");
        assert!(sup.stop(Duration::from_secs(10)));
        t.join().unwrap();
    }

    #[test]
    fn a_program_that_isnt_there_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(config(d.path(), d.path().join("missing")));
        let t = sup.spawn();
        let s = wait_for(&sup, |s| matches!(s, State::Stopped { .. }));
        assert!(
            matches!(&s, State::Stopped { why } if why.starts_with("flux-server didn't start")),
            "{s:?}"
        );
        sup.stop(Duration::from_secs(1));
        t.join().unwrap();
    }

    #[test]
    fn says_how_flux_stopped() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            describe(&ExitStatus::from_raw(1 << 8)),
            "flux stopped (exit status 1)"
        );
        assert_eq!(describe(&ExitStatus::from_raw(9)), "flux stopped (signal 9)");
    }
}
