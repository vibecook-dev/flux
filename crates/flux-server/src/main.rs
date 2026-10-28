//! flux media server binary: HTTP API, media delivery, background jobs.
//! `flux-server --help` for the flags.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use flux_server::config::Command;
use flux_server::service::{ServiceArgs, ServiceCommand};
use flux_server::update::Restart;
use flux_server::{Cli, Config};
use tracing_subscriber::EnvFilter;

mod alloc;
#[cfg(windows)]
mod winservice;

fn main() -> ExitCode {
    // First: SQLite's allocator can only be set before its first use.
    alloc::install();
    // Before any helper starts (`--check` runs ffmpeg too).
    #[cfg(windows)]
    let windows = WindowsSetup::run();
    let t0 = Instant::now();
    let mut cli = Cli::parse();
    let check = cli.check;
    let command = cli.command.take();
    // Before resolving: the flags as given, for a service to run with.
    let service_args = ServiceArgs::of(&cli);
    let cfg = match Config::resolve(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("flux-server: {e:#}");
            return ExitCode::from(2);
        }
    };
    match &command {
        Some(Command::Update(args)) => return flux_server::update::cli::run(&cfg, args),
        Some(Command::OwnerLink) => return flux_server::devices::cli(&cfg),
        Some(Command::Service {
            action: ServiceCommand::Run,
        }) => {
            #[cfg(windows)]
            return winservice::run(service_args.name, cfg, t0, windows);
            #[cfg(not(windows))]
            {
                eprintln!("flux-server: `service run` is what the Windows service manager starts; run flux-server itself");
                return ExitCode::from(2);
            }
        }
        Some(Command::Service { action }) => return flux_server::service::cli(*action, &service_args, &cfg),
        None => {}
    }
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
    init_log(&cfg.log, None);
    log_start(&cfg);
    #[cfg(windows)]
    windows.log();
    let (code, restart) = serve(cfg, t0, wait_for_signal());
    match restart {
        Some(r) => start_again(&r),
        None => code,
    }
}

/// The log: to stderr (coloured on a terminal), or to `file` (a Windows
/// service has no stderr).
fn init_log(level: &str, file: Option<std::fs::File>) {
    let filter = EnvFilter::try_new(log_filter(level)).unwrap_or_else(|_| EnvFilter::new(log_filter("info")));
    match file {
        None => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_ansi(log_colours(
                std::io::stderr().is_terminal(),
                std::env::var_os("NO_COLOR"),
            ))
            .init(),
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init(),
    }
}

/// The first lines of the log: the version and the data dir.
fn log_start(cfg: &Config) {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = %cfg.data_dir.display(),
        "starting"
    );
    if let Some(older) = &cfg.older_data_dir {
        tracing::warn!(
            "{} holds a catalog from an earlier flux (the default data dir on Windows up to \
             0.1.0-alpha.2), but this run uses {}. To keep its libraries, artwork and watched and \
             resume state, stop flux, then pass --data-dir \"{}\" or move that folder's contents \
             here; delete it if you don't need it",
            older.display(),
            cfg.data_dir.display(),
            older.display()
        );
    }
}

/// Serves until `stop` (a signal, or the Windows service manager), or until
/// flux asks itself to restart (after an update), then stops within that
/// stop's limits: the exit status, and the restart that was asked for.
fn serve(
    cfg: Config,
    t0: Instant,
    stop: impl std::future::Future<Output = Stop>,
) -> (ExitCode, Option<Restart>) {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("cannot start the runtime: {e}");
            return (ExitCode::FAILURE, None);
        }
    };
    let (code, runtime_grace, restart) = rt.block_on(async move {
        let server = match flux_server::start_at(cfg, t0).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("cannot start: {e:#}");
                return (ExitCode::FAILURE, Duration::from_secs(2), None);
            }
        };
        let (stop, restart) = tokio::select! {
            stop = stop => (stop, None),
            restart = server.state.updater.restart_asked() => (Stop::Asked("restart"), Some(restart)),
        };
        tracing::info!(signal = stop.name(), "shutting down");
        let budget = stop.budget();
        if let Some(deadline) = budget.deadline {
            exit_watchdog(deadline, stop.name(), WINDOWS_ENDS_IT);
        }
        // Ours is then the last reference, so the state's drop (the
        // catalog's and the scanner's, which join threads) runs after "flux
        // stopped", with the exit watchdog armed.
        let state = server.state.clone();
        server.shutdown(budget.grace).await;
        exit_watchdog(budget.exit, "\"flux stopped\"", "a shutdown bug");
        drop(state);
        (ExitCode::SUCCESS, budget.runtime, restart)
    });
    // Dropping the runtime would wait for every blocking task, and one stuck
    // in a read (a stalled NAS, an ffprobe that ignores its kill) would keep
    // the process alive after "flux stopped" (review ROB-1).
    rt.shutdown_timeout(runtime_grace);
    (code, restart)
}

/// The exit status that asks a service manager to start flux again: what a
/// restart ends with when flux can't (or, under a Windows service, shouldn't)
/// start the new process itself. `EX_TEMPFAIL`.
const RESTART_BY_SUPERVISOR: u8 = 75;

/// Starts `r.exe` with this process's arguments once the server has stopped:
/// in this process's place on Unix (the same PID, so systemd and launchd
/// keep following it), as a new process on Windows, unless a service manager
/// runs flux there (it starts flux again after the exit status).
fn start_again(r: &Restart) -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    tracing::info!(exe = %r.exe.display(), why = r.why, "starting again");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only returns when it fails.
        let err = std::process::Command::new(&r.exe).args(&args).exec();
        tracing::error!(error = %err, exe = %r.exe.display(), "cannot start flux again");
        ExitCode::from(RESTART_BY_SUPERVISOR)
    }
    #[cfg(not(unix))]
    {
        if supervised() {
            tracing::info!("exiting for the service manager to start flux again");
            return ExitCode::from(RESTART_BY_SUPERVISOR);
        }
        // The new flux is started in flux's helper job; it must outlive this one.
        #[cfg(windows)]
        if let Err(e) = flux_server::win32::let_children_outlive_this_process() {
            tracing::warn!(error = %e, "the new flux may end with this one");
        }
        match std::process::Command::new(&r.exe).args(&args).spawn() {
            Ok(child) => {
                tracing::info!(pid = child.id(), "flux started again");
                ExitCode::SUCCESS
            }
            Err(e) => {
                tracing::error!(error = %e, exe = %r.exe.display(), "cannot start flux again");
                ExitCode::from(RESTART_BY_SUPERVISOR)
            }
        }
    }
}

/// A service manager (or a service wrapper such as NSSM) runs flux: it
/// restarts flux itself after the exit status, and a process flux started
/// would escape it. `FLUX_RESTART=exit` or `spawn` says so outright;
/// otherwise a process in session 0 (where services run) is supervised.
#[cfg(not(unix))]
fn supervised() -> bool {
    match std::env::var("FLUX_RESTART").as_deref() {
        Ok("exit") => true,
        Ok("spawn") => false,
        _ => {
            #[cfg(windows)]
            {
                flux_server::win32::in_service_session()
            }
            #[cfg(not(windows))]
            {
                false
            }
        }
    }
}

/// Why flux is stopping, which bounds how long the stop may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// Ctrl-C (also how NSSM and WinSW stop a service), SIGTERM or
    /// Ctrl-Break: only flux's own limits apply.
    Asked(&'static str),
    /// Windows is closing flux's console. It ends the process about 5 s
    /// after the event, whatever flux is doing, so the whole stop has to fit
    /// in that. (Logoff and shutdown would be handled the same way, but
    /// Windows doesn't deliver them to flux: see `ConsoleEvents`.)
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

/// Why a stop with a deadline ends there: only a closing console has one.
const WINDOWS_ENDS_IT: &str = "Windows ends flux 5 s after its console closes";

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

    /// `next` arrived while this stop was running: the deadline it adds
    /// from now, if any. Only a closing console bounds a stop that had no
    /// deadline; a repeated Ctrl-C changes nothing, as on Unix.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn then(self, next: Stop) -> Option<Duration> {
        match (self.budget().deadline, next.budget().deadline) {
            (None, Some(deadline)) => Some(deadline),
            _ => None,
        }
    }
}

/// A backstop, not a fix: if the process still runs `after` from now, it
/// logs that and exits with status 1. After "flux stopped" the listeners
/// are closed and the catalog committed, so a stuck drop or exit handler
/// only costs the wait for a supervisor's SIGKILL (Docker's 10 s, systemd's
/// 90 s); the error and the status keep such a bug visible. `since` names
/// what happened `after` ago, and `why` why flux can't wait any longer.
fn exit_watchdog(after: Duration, since: &'static str, why: &'static str) {
    let spawned = std::thread::Builder::new()
        .name("flux-exit-watchdog".into())
        .stack_size(64 << 10)
        .spawn(move || {
            std::thread::sleep(after);
            tracing::error!(
                "still running {:.1} s after {since} ({why}); exiting",
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

/// The first console control event (see [`ConsoleEvents`]). flux keeps
/// listening until the process ends, so a second event can't cut the stop
/// short.
#[cfg(windows)]
async fn wait_for_signal() -> Stop {
    match (ConsoleEvents::listen(), ConsoleEvents::listen()) {
        (Ok(mut events), Ok(held)) => {
            // tokio's handler takes an event while any listener for it is
            // left, and Windows' default handler, which ends flux on the
            // spot (exit 0xC000013A), runs only once none is. `held` is
            // never read and never dropped, so that holds until the process
            // ends, also after the runtime has dropped the task below:
            // tokio then ignores a Ctrl-C and keeps a closing console's
            // event until flux exits (or Windows ends it 5 s later).
            std::mem::forget(held);
            let stop = events.next().await;
            tokio::spawn(events.during_the_stop(stop));
            stop
        }
        (Err(e), _) | (_, Err(e)) => {
            tracing::warn!(
                error = %e,
                "cannot listen for console events: Ctrl-Break or closing the console ends flux at once"
            );
            let _ = tokio::signal::ctrl_c().await;
            Stop::Asked("Ctrl-C")
        }
    }
}

/// Listeners for every console control event: Ctrl-C (what NSSM and WinSW
/// send to stop a service), Ctrl-Break, and the console closing. Without a
/// handler the last two end the process on the spot, with no "flux stopped"
/// and the catalog's last writes lost.
///
/// Logoff and shutdown are listened for too, but Windows doesn't deliver
/// them: it calls no handler for them in a process that has loaded
/// user32.dll, which flux has (through shell32), and in a user's session
/// they reach only services anyway. So at logoff or shutdown flux stops
/// gracefully only if a service wrapper sends it Ctrl-C first.
#[cfg(windows)]
struct ConsoleEvents {
    ctrl_c: tokio::signal::windows::CtrlC,
    ctrl_break: tokio::signal::windows::CtrlBreak,
    close: tokio::signal::windows::CtrlClose,
    shutdown: tokio::signal::windows::CtrlShutdown,
    logoff: tokio::signal::windows::CtrlLogoff,
}

#[cfg(windows)]
impl ConsoleEvents {
    fn listen() -> std::io::Result<ConsoleEvents> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
        Ok(ConsoleEvents {
            ctrl_c: ctrl_c()?,
            ctrl_break: ctrl_break()?,
            close: ctrl_close()?,
            shutdown: ctrl_shutdown()?,
            logoff: ctrl_logoff()?,
        })
    }

    async fn next(&mut self) -> Stop {
        tokio::select! {
            _ = self.ctrl_c.recv() => Stop::Asked("Ctrl-C"),
            _ = self.ctrl_break.recv() => Stop::Asked("Ctrl-Break"),
            _ = self.close.recv() => Stop::Closing("console closed"),
            _ = self.shutdown.recv() => Stop::Closing("shutdown"),
            _ = self.logoff.recv() => Stop::Closing("logoff"),
        }
    }

    /// The events after `first`, until the process ends: the stop carries
    /// on. A closing console also bounds a stop that had no deadline, so
    /// that flux, not Windows, ends it if it runs long.
    async fn during_the_stop(mut self, first: Stop) {
        let mut stop = first;
        loop {
            let next = self.next().await;
            match stop.then(next) {
                Some(deadline) => {
                    tracing::warn!(
                        signal = next.name(),
                        "already stopping; flux exits within {:.0} s, before Windows ends it",
                        deadline.as_secs_f64()
                    );
                    exit_watchdog(deadline, next.name(), WINDOWS_ENDS_IT);
                    stop = next;
                }
                None => tracing::info!(signal = next.name(), "already stopping"),
            }
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

/// The log filter for `--log` / `RUST_LOG`. The mp4-atom crate logs every
/// box it does not model (`udta/name`, `gmhd`, `iods`, `btrt`, `ilst/©nam`
/// … which ffmpeg and most muxers write) at WARN: once per video transcode
/// run and per MP4 indexed. Those boxes are skipped harmlessly, and a file
/// mp4-atom cannot read still fails with an error flux logs itself, so its
/// notices are treated as debug output: shown when the filter enables
/// debug (or trace) globally or names `mp4_atom`, hidden otherwise.
fn log_filter(spec: &str) -> String {
    let directives: Vec<&str> = spec.split(',').map(str::trim).filter(|d| !d.is_empty()).collect();
    let verbose = directives.iter().any(|d| {
        d.starts_with("mp4_atom")
            || (!d.contains('=') && matches!(d.to_ascii_lowercase().as_str(), "debug" | "trace" | "off"))
    });
    if verbose || directives.is_empty() {
        spec.to_string()
    } else {
        format!("{},mp4_atom=error", directives.join(","))
    }
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

    /// A second Ctrl-C during a stop changes nothing (as on Unix); the
    /// console closing during a Ctrl-C stop bounds it to Windows' 5 s.
    /// tests/console.rs sends a second Ctrl-Break to a running
    /// flux-server.exe.
    #[test]
    fn a_second_event_bounds_a_stop_only_when_the_console_closes() {
        let (asked, closing) = (Stop::Asked("Ctrl-C"), Stop::Closing("console closed"));
        assert_eq!(asked.then(Stop::Asked("Ctrl-Break")), None);
        assert_eq!(asked.then(closing), closing.budget().deadline);
        assert!(asked.then(closing).is_some_and(|d| d < Duration::from_secs(5)));
        assert_eq!(closing.then(asked), None);
        assert_eq!(closing.then(closing), None);
    }

    #[test]
    fn mp4_atom_box_notices_are_debug_output() {
        assert_eq!(log_filter("info"), "info,mp4_atom=error");
        assert_eq!(
            log_filter("info, flux_hls=debug"),
            "info,flux_hls=debug,mp4_atom=error"
        );
        // Asked for: debug everywhere, or mp4_atom by name.
        assert_eq!(log_filter("debug"), "debug");
        assert_eq!(log_filter("info,mp4_atom=warn"), "info,mp4_atom=warn");
        assert_eq!(log_filter("off"), "off");
        // What the filter does with it.
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::{Layer, SubscriberExt};
        let seen = |spec: &str| {
            let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let h = hits.clone();
            let count = tracing_subscriber::fmt::layer()
                .with_writer(move || {
                    h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    std::io::sink()
                })
                .with_filter(tracing_subscriber::EnvFilter::new(log_filter(spec)));
            with_default(tracing_subscriber::registry().with(count), || {
                tracing::warn!(target: "mp4_atom::error", kind = "gmhd", "unexpected box");
                tracing::warn!(target: "flux_hls::engine", "a real warning");
            });
            hits.load(std::sync::atomic::Ordering::Relaxed)
        };
        assert_eq!(seen("info"), 1, "only flux's own warning");
        assert_eq!(seen("debug"), 2);
    }
}
