//! flux as a Windows service: `flux-server service run`, what the service
//! manager starts (`flux_server::service` installs it, and so does the
//! Windows installer). The server runs as it does in a terminal, except:
//!
//! - the service manager's Stop, or the machine shutting down, stops it,
//!   within the same limits as Ctrl-C;
//! - it logs to `flux.log` in the data dir (a service has no stderr); an
//!   old log over 10 MiB is kept as `flux.log.1`;
//! - a restart (after an update) is an exit with status 75, which the
//!   service manager answers by starting flux again: the failure actions
//!   say to, also for an exit that wasn't a crash.
// `define_windows_service!` expands to an `extern "system" fn` that reads
// the service manager's arguments through a raw pointer.
#![allow(unsafe_code)]

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::{init_log, log_start, serve, Stop, WindowsSetup, RESTART_BY_SUPERVISOR};
use flux_server::Config;

/// What `run` hands the service's main function, which the service manager
/// calls on a thread of its own.
static START: Mutex<Option<(String, Config, Instant, WindowsSetup)>> = Mutex::new(None);

/// A log over this size is kept as `flux.log.1` when flux starts.
const LOG_ROTATE_BYTES: u64 = 10 << 20;

/// Hands this process to the service manager, which runs [`service_main`]
/// and returns once the service called `name` has stopped.
pub fn run(name: String, cfg: Config, t0: Instant, windows: WindowsSetup) -> ExitCode {
    *START.lock().unwrap_or_else(|p| p.into_inner()) = Some((name.clone(), cfg, t0, windows));
    match service_dispatcher::start(&name, ffi_service_main) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "flux-server: `service run` is what the Windows service manager starts ({e}). To run flux in \
                 this terminal, run flux-server without it; to install the service, `flux-server service install`."
            );
            ExitCode::from(2)
        }
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    let Some((name, cfg, t0, windows)) = START.lock().unwrap_or_else(|p| p.into_inner()).take() else {
        return;
    };
    let stopping = CancellationToken::new();
    let asked = stopping.clone();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
            asked.cancel();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(status) = service_control_handler::register(&name, handler) else {
        return;
    };
    let report = move |state: ServiceState, exit_code: ServiceExitCode, wait: Duration| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code,
            checkpoint: 0,
            wait_hint: wait,
            process_id: None,
        });
    };
    // Running at once: the service manager (and the installer, which waits
    // for it) gives a service 30 s, and a big catalog's first scan is longer.
    report(ServiceState::Running, ServiceExitCode::NO_ERROR, Duration::ZERO);

    // Without its file the log goes nowhere, but flux still runs.
    init_log(&cfg.log, open_log(&cfg.data_dir).ok());
    tracing::info!(service = %name, "running as a Windows service");
    log_start(&cfg);
    windows.log();
    restart_on_any_exit(&name);

    let stop = async move {
        stopping.cancelled().await;
        report(
            ServiceState::StopPending,
            ServiceExitCode::NO_ERROR,
            Duration::from_secs(20),
        );
        Stop::Asked("service stop")
    };
    let (code, restart) = serve(cfg, t0, stop);
    let exit = if restart.is_some() {
        tracing::info!("exiting for the service manager to start flux again");
        ServiceExitCode::ServiceSpecific(RESTART_BY_SUPERVISOR.into())
    } else if code == ExitCode::SUCCESS {
        ServiceExitCode::NO_ERROR
    } else {
        ServiceExitCode::ServiceSpecific(1)
    };
    report(ServiceState::Stopped, exit, Duration::ZERO);
}

/// `flux.log` in the data dir, appended to; one over [`LOG_ROTATE_BYTES`] is
/// kept as `flux.log.1` first.
fn open_log(data_dir: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join("flux.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(&path, data_dir.join("flux.log.1"));
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

/// The failure actions start a service again after it fails; an exit with a
/// status (flux asking for a restart) counts only when this flag is on. The
/// Windows installer can't set it, so flux does, each time it starts.
fn restart_on_any_exit(name: &str) {
    let set = || -> windows_service::Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service =
            manager.open_service(name, ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_CONFIG)?;
        if !service.get_failure_actions_on_non_crash_failures()? {
            service.set_failure_actions_on_non_crash_failures(true)?;
        }
        Ok(())
    };
    if let Err(e) = set() {
        tracing::warn!(error = %e, "can't make the service manager start flux again after an update");
    }
}
