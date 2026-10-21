//! flux media server binary: HTTP API, media delivery, background jobs.
//! `flux-server --help` for the flags.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use flux_server::{Cli, Config};
use tracing_subscriber::EnvFilter;

mod alloc;

fn main() -> ExitCode {
    // First: SQLite's allocator can only be set before its first use.
    alloc::install();
    // Before any helper starts (`--check` runs ffmpeg too).
    #[cfg(windows)]
    let windows = WindowsSetup::run();
    let t0 = Instant::now();
    let cli = Cli::parse();
    let check = cli.check;
    let cfg = match Config::resolve(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("flux-server: {e:#}");
            return ExitCode::from(2);
        }
    };
    if check {
        // `--check`: the paths flux would use, what ffmpeg/ffprobe can do
        // here, then exit.
        let report = flux_server::doctor::ToolReport::detect(
            &cfg.ffmpeg,
            &cfg.ffprobe,
            cfg.video_encoder,
            &cfg.vaapi_device,
        );
        print!("{}", report.text_with(&flux_server::doctor::config_text(&cfg)));
        return if report.ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let filter = EnvFilter::try_new(&cfg.log).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(log_colours(
            std::io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR"),
        ))
        .init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = %cfg.data_dir.display(),
        "starting"
    );

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("flux-server: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(windows)]
    windows.log();
    let (code, runtime_grace) = rt.block_on(async move {
        let server = match flux_server::start_at(cfg, t0).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("cannot start: {e:#}");
                return (ExitCode::FAILURE, Duration::from_secs(2));
            }
        };
        let stop = wait_for_signal().await;
        tracing::info!(signal = stop.name(), "shutting down");
        let budget = stop.budget();
        if let Some(deadline) = budget.deadline {
            exit_watchdog(deadline, stop.name());
        }
        // Ours is then the last reference, so the state's drop (the
        // catalog's and the scanner's, which join threads) runs after "flux
        // stopped", with the exit watchdog armed.
        let state = server.state.clone();
        server.shutdown(budget.grace).await;
        exit_watchdog(budget.exit, "\"flux stopped\"");
        drop(state);
        (ExitCode::SUCCESS, budget.runtime)
    });
    // Dropping the runtime would wait for every blocking task, and one stuck
    // in a read (a stalled NAS, an ffprobe that ignores its kill) would keep
    // the process alive after "flux stopped" (review ROB-1).
    rt.shutdown_timeout(runtime_grace);
    code
}

/// Why flux is stopping, which bounds how long the stop may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// Ctrl-C (also how NSSM and WinSW stop a service), SIGTERM or
    /// Ctrl-Break: only flux's own limits apply.
    Asked(&'static str),
    /// Windows is closing flux's console, logging the user off or shutting
    /// down. It ends the process about 5 s after the event, whatever flux
    /// is doing, so the whole stop has to fit in that.
    #[cfg_attr(not(windows), allow(dead_code))]
    Closing(&'static str),
}

/// The time limits of one stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Budget {
    /// In-flight requests get this long, then the scanner gets as long.
    grace: Duration,
    /// From the signal to a forced exit, if bounded.
    deadline: Option<Duration>,
    /// From "flux stopped" to a forced exit: the state's drop plus the
    /// runtime's blocking tasks.
    exit: Duration,
    /// The runtime's blocking tasks.
    runtime: Duration,
}

/// How long the process may take to end after "flux stopped" (a stop that
/// was asked for): the state's drop plus the runtime's 2 s.
const EXIT_DEADLINE: Duration = Duration::from_secs(5);

impl Stop {
    fn name(self) -> &'static str {
        match self {
            Stop::Asked(name) | Stop::Closing(name) => name,
        }
    }

    fn budget(self) -> Budget {
        match self {
            Stop::Asked(_) => Budget {
                grace: Duration::from_secs(5),
                deadline: None,
                exit: EXIT_DEADLINE,
                runtime: Duration::from_secs(2),
            },
            // Ends on its own before Windows' 5 s, with "flux stopped" and
            // the catalog committed well before that.
            Stop::Closing(_) => Budget {
                grace: Duration::from_secs(1),
                deadline: Some(Duration::from_secs(4)),
                exit: Duration::from_millis(1500),
                runtime: Duration::from_millis(500),
            },
        }
    }
}

/// A backstop, not a fix: if the process still runs `after` from now, it
/// logs that and exits with status 1. After "flux stopped" the listeners
/// are closed and the catalog committed, so a stuck drop or exit handler
/// only costs the wait for a supervisor's SIGKILL (Docker's 10 s, systemd's
/// 90 s); the error and the status keep such a bug visible. `since` names
/// what happened `after` ago.
fn exit_watchdog(after: Duration, since: &'static str) {
    let spawned = std::thread::Builder::new()
        .name("flux-exit-watchdog".into())
        .stack_size(64 << 10)
        .spawn(move || {
            std::thread::sleep(after);
            tracing::error!(
                "still running {:.1} s after {since} (a shutdown bug); exiting",
                after.as_secs_f64()
            );
            hard_exit(1);
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "exit watchdog did not start");
    }
}

/// Ends the process at once: no atexit handlers or thread-local
/// destructors, which could be what hangs, and no waiting for another
/// thread already in `exit` (`std::process::exit` waits for it).
#[allow(unsafe_code)]
fn hard_exit(code: i32) -> ! {
    #[cfg(unix)]
    // SAFETY: `_exit` takes no pointers and does not return.
    unsafe {
        libc::_exit(code)
    }
    #[cfg(windows)]
    flux_server::win32::terminate_this_process(code as u32);
    #[cfg(not(any(unix, windows)))]
    std::process::exit(code)
}

/// SIGINT (Ctrl-C) or SIGTERM.
#[cfg(unix)]
async fn wait_for_signal() -> Stop {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => Stop::Asked("SIGINT"),
                _ = term.recv() => Stop::Asked("SIGTERM"),
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            Stop::Asked("SIGINT")
        }
    }
}

/// Every console control event: Ctrl-C (what NSSM and WinSW send to stop a
/// service), Ctrl-Break, and the console closing, a logoff or a shutdown.
/// Without a handler the last four end the process on the spot, with no
/// "flux stopped" and the catalog's last writes lost.
#[cfg(windows)]
async fn wait_for_signal() -> Stop {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
    match (
        ctrl_c(),
        ctrl_break(),
        ctrl_close(),
        ctrl_shutdown(),
        ctrl_logoff(),
    ) {
        (Ok(mut c), Ok(mut brk), Ok(mut close), Ok(mut shutdown), Ok(mut logoff)) => {
            tokio::select! {
                _ = c.recv() => Stop::Asked("Ctrl-C"),
                _ = brk.recv() => Stop::Asked("Ctrl-Break"),
                _ = close.recv() => Stop::Closing("console closed"),
                _ = shutdown.recv() => Stop::Closing("shutdown"),
                _ = logoff.recv() => Stop::Closing("logoff"),
            }
        }
        _ => {
            let _ = tokio::signal::ctrl_c().await;
            Stop::Asked("Ctrl-C")
        }
    }
}

#[cfg(not(any(unix, windows)))]
async fn wait_for_signal() -> Stop {
    let _ = tokio::signal::ctrl_c().await;
    Stop::Asked("Ctrl-C")
}

/// Windows process setup, done first; failures are logged once the log
/// is up.
#[cfg(windows)]
struct WindowsSetup {
    job: std::io::Result<()>,
    ctrl_c: std::io::Result<()>,
}

#[cfg(windows)]
impl WindowsSetup {
    fn run() -> WindowsSetup {
        WindowsSetup {
            job: flux_server::win32::end_helpers_with_this_process(),
            ctrl_c: flux_server::win32::accept_ctrl_c(),
        }
    }

    fn log(&self) {
        if let Err(e) = &self.job {
            tracing::warn!(
                error = %e,
                "no job object: if flux is killed, its ffmpeg and ffprobe processes keep running"
            );
        }
        if let Err(e) = &self.ctrl_c {
            tracing::warn!(error = %e, "cannot turn Ctrl-C on: stop flux with Ctrl-Break");
        }
    }
}

/// Whether the log on stderr gets colour codes: only on a terminal, and not
/// when `NO_COLOR` is set to anything but "" (<https://no-color.org>). Under
/// systemd, `docker logs` or a redirect, stderr is a pipe or a file, and the
/// codes would end up in the log as text.
fn log_colours(stderr_is_terminal: bool, no_color: Option<OsString>) -> bool {
    stderr_is_terminal && no_color.is_none_or(|v| v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_colours_only_on_a_terminal_without_no_color() {
        assert!(log_colours(true, None));
        assert!(log_colours(true, Some("".into())));
        assert!(!log_colours(true, Some("1".into())));
        assert!(!log_colours(false, None));
        assert!(!log_colours(false, Some("".into())));
        assert!(!log_colours(false, Some("1".into())));
    }

    /// Windows ends a process about 5 s after it closes its console (or
    /// logs off, or shuts down). Before, only Ctrl-C was handled there, and
    /// those events killed flux without "flux stopped".
    #[test]
    fn a_closing_console_stop_fits_in_windows_five_seconds() {
        let asked = Stop::Asked("SIGTERM").budget();
        assert_eq!(asked.grace, Duration::from_secs(5));
        assert_eq!((asked.deadline, asked.exit), (None, EXIT_DEADLINE));
        let closing = Stop::Closing("console closed").budget();
        let deadline = closing.deadline.expect("bounded");
        assert!(deadline < Duration::from_secs(5), "{deadline:?}");
        // Requests, then the scanner, then the drop and the runtime.
        assert!(2 * closing.grace + closing.exit <= deadline, "{closing:?}");
        assert!(closing.runtime < closing.exit, "{closing:?}");
        assert_eq!(Stop::Closing("logoff").name(), "logoff");
    }
}
