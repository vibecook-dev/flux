//! flux media server binary: HTTP API, media delivery, background jobs.
//! `flux-server --help` for the flags.

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
