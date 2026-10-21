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
        // `--check`: what ffmpeg/ffprobe can do here, then exit.
        let report = flux_server::doctor::ToolReport::detect(
            &cfg.ffmpeg,
            &cfg.ffprobe,
            cfg.video_encoder,
            &cfg.vaapi_device,
        );
        print!("{}", report.text());
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

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("flux-server: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = rt.block_on(async move {
        let server = match flux_server::start_at(cfg, t0).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("cannot start: {e:#}");
                return ExitCode::FAILURE;
            }
        };
        wait_for_signal().await;
        tracing::info!("shutting down");
        server.shutdown(Duration::from_secs(5)).await;
        ExitCode::SUCCESS
    });
    // Dropping the runtime would wait for every blocking task, and one stuck
    // in a read (a stalled NAS, an ffprobe that ignores its kill) would keep
    // the process alive after "flux stopped" (review ROB-1).
    rt.shutdown_timeout(Duration::from_secs(2));
    code
}

/// SIGINT (Ctrl-C) or SIGTERM.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
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
    use super::log_colours;

    #[test]
    fn log_colours_only_on_a_terminal_without_no_color() {
        assert!(log_colours(true, None));
        assert!(log_colours(true, Some("".into())));
        assert!(!log_colours(true, Some("1".into())));
        assert!(!log_colours(false, None));
        assert!(!log_colours(false, Some("".into())));
        assert!(!log_colours(false, Some("1".into())));
    }
}
