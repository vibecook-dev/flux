//! Windows: flux as a Windows service (`service`), run as LocalSystem. It
//! starts with Windows (a little after boot), and the service manager starts
//! it again when it stops on its own: after a crash, and after an update,
//! when flux exits for the new version to be started (`main`'s
//! `RESTART_BY_SUPERVISOR`). The service manager runs `flux-server service
//! run`, which `main`'s `winservice` answers.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SERVICE_NOT_ACTIVE,
};

use super::{ServiceArgs, ServiceCommand};
use crate::config::Config;

/// The data dir of a LocalSystem service told no other: LocalSystem's own
/// %LOCALAPPDATA% is deep in System32.
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("flux")
}

/// Start it again 5 s after it stops on its own, every time (the count
/// resets after a day without a failure).
pub fn failure_actions() -> ServiceFailureActions {
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            };
            3
        ]),
    }
}

/// The error code of a failed Win32 call, if that's what it was.
fn code(e: &windows_service::Error) -> Option<u32> {
    match e {
        windows_service::Error::Winapi(io) => io.raw_os_error().map(|c| c as u32),
        _ => None,
    }
}

fn say(e: windows_service::Error) -> String {
    if code(&e) == Some(ERROR_ACCESS_DENIED) {
        "Windows refused: run this in a terminal opened as administrator.".into()
    } else {
        e.to_string()
    }
}

/// Waits (up to `within`) for the service to reach `state`.
fn wait_for(service: &windows_service::service::Service, state: ServiceState, within: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        match service.query_status() {
            Ok(s) if s.current_state == state => return true,
            Ok(_) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return false,
        }
    }
    false
}

pub fn run(command: ServiceCommand, args: &ServiceArgs, _cfg: &Config) -> Result<String, String> {
    let name = args.name.clone();
    match command {
        ServiceCommand::Install => install(&name, args),
        ServiceCommand::Uninstall => uninstall(&name),
        ServiceCommand::Start => {
            let manager =
                ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(say)?;
            let service =
                match manager.open_service(&name, ServiceAccess::START | ServiceAccess::QUERY_STATUS) {
                    Ok(s) => s,
                    Err(e) if code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
                        return Err("the flux service isn't installed: `flux-server service install`".into())
                    }
                    Err(e) => return Err(say(e)),
                };
            match service.start(&[] as &[&OsStr]) {
                Ok(()) => {}
                Err(e) if code(&e) == Some(ERROR_SERVICE_ALREADY_RUNNING) => {
                    return Ok("flux is running already.".into())
                }
                Err(e) => return Err(say(e)),
            }
            if wait_for(&service, ServiceState::Running, Duration::from_secs(20)) {
                Ok("flux started.".into())
            } else {
                Ok("flux is starting.".into())
            }
        }
        ServiceCommand::Stop => {
            let manager =
                ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(say)?;
            let service = match manager.open_service(&name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)
            {
                Ok(s) => s,
                Err(e) if code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
                    return Ok("The flux service isn't installed.".into())
                }
                Err(e) => return Err(say(e)),
            };
            match service.stop() {
                Ok(_) => {}
                Err(e) if code(&e) == Some(ERROR_SERVICE_NOT_ACTIVE) => {
                    return Ok("flux isn't running.".into())
                }
                Err(e) => return Err(say(e)),
            }
            wait_for(&service, ServiceState::Stopped, Duration::from_secs(30));
            Ok("flux stopped; it starts again with Windows, or with `flux-server service start`.".into())
        }
        ServiceCommand::Status => {
            let manager =
                ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(say)?;
            let service = match manager
                .open_service(&name, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG)
            {
                Ok(s) => s,
                Err(e) if code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
                    return Ok("The flux service isn't installed (`flux-server service install`).".into())
                }
                Err(e) => return Err(say(e)),
            };
            let status = service.query_status().map_err(say)?;
            let state = match (status.current_state, status.process_id) {
                (ServiceState::Running, Some(pid)) => format!("running (pid {pid})"),
                (ServiceState::Running, None) => "running".into(),
                (ServiceState::Stopped, _) => "stopped".into(),
                (ServiceState::StartPending, _) => "starting".into(),
                (ServiceState::StopPending, _) => "stopping".into(),
                (other, _) => format!("{other:?}").to_lowercase(),
            };
            let runs = service
                .query_config()
                .map(|c| c.executable_path.display().to_string())
                .unwrap_or_default();
            Ok(format!(
                "flux service: {state}\n  runs: {runs}\n  log: flux.log in its data dir"
            ))
        }
        ServiceCommand::Run => Err("`service run` is what the Windows service manager starts".into()),
    }
}

/// Whether `path` is in the user's profile (`%USERPROFILE%`), where programs
/// running as that user can change it without asking for administrator
/// rights.
pub fn in_user_profile(path: &std::path::Path) -> bool {
    let Some(profile) = std::env::var_os("USERPROFILE").filter(|p| !p.is_empty()) else {
        return false;
    };
    let lower = |p: &std::path::Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let (path, profile) = (lower(path), lower(std::path::Path::new(&profile)));
    path == profile || path.starts_with(&format!("{profile}\\"))
}

fn install(name: &str, args: &ServiceArgs) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // The service runs as LocalSystem: its program, and the config file that
    // can name the ffmpeg it runs, must be where only administrators can
    // change them, or anything running as this user could make itself
    // SYSTEM through them.
    if in_user_profile(&exe) {
        return Err(format!(
            "{} is in your user folder, where any program you run can replace it, and the service would \
             run it as SYSTEM. Move the flux folder to C:\\Program Files\\flux (as administrator), then run \
             `& 'C:\\Program Files\\flux\\flux-server.exe' service install` from there.",
            exe.display()
        ));
    }
    if let Some(config) = args.config.as_ref().filter(|c| in_user_profile(c)) {
        return Err(format!(
            "{} is in your user folder, where any program you run can change it (its ffmpeg, say), and the \
             service would follow it as SYSTEM. Put the config file in C:\\ProgramData\\flux (as \
             administrator) and pass that.",
            config.display()
        ));
    }
    let mut args = args.clone();
    // A data dir LocalSystem and its owner both find, unless one was given:
    // C:\ProgramData\flux, for SYSTEM and Administrators only, as the Windows
    // installer leaves it. ProgramData lets every user read and add files,
    // and the log has owner links.
    if args.config.is_none() && args.data_dir.is_none() {
        let dir = default_data_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        crate::win32::restrict_to_admins(&dir)
            .map_err(|e| format!("leaving {} to administrators: {e}", dir.display()))?;
        args.data_dir = Some(dir);
    }
    let mut launch: Vec<OsString> = vec!["service".into(), "run".into()];
    launch.extend(args.args());
    let info = ServiceInfo {
        name: name.into(),
        display_name: "flux media server".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: launch,
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(say)?;
    let access = ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::QUERY_STATUS;
    // Installed already: point it at this flux and these flags.
    let service = match manager.open_service(name, access) {
        Ok(existing) => {
            if existing.stop().is_ok() {
                wait_for(&existing, ServiceState::Stopped, Duration::from_secs(30));
            }
            existing.change_config(&info).map_err(say)?;
            existing
        }
        Err(e) if code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            manager.create_service(&info, access).map_err(say)?
        }
        Err(e) => return Err(say(e)),
    };
    let _ = service.set_description(
        "flux: browse, play and stream your films and shows (https://github.com/vibecook-dev/flux)",
    );
    service.set_delayed_auto_start(true).map_err(say)?;
    service.update_failure_actions(failure_actions()).map_err(say)?;
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(say)?;
    service.start(&[] as &[&OsStr]).map_err(say)?;
    let running = wait_for(&service, ServiceState::Running, Duration::from_secs(20));
    let data = args
        .data_dir
        .as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "the config file's".into());
    Ok(format!(
        "flux runs as a Windows service now ({}): it starts with Windows, and again if it stops on its own.\n  runs: {}\n  data dir: {data} (its log: flux.log)\nOpen http://localhost:{}",
        if running { "running" } else { "starting" },
        exe.display(),
        args.port.unwrap_or(8096)
    ))
}

fn uninstall(name: &str) -> Result<String, String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(say)?;
    let service = match manager.open_service(
        name,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(s) => s,
        Err(e) if code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            return Ok("No flux service was installed.".into())
        }
        Err(e) => return Err(say(e)),
    };
    // Marked for deletion; gone once it's stopped and no handle is open.
    service.delete().map_err(say)?;
    if service
        .query_status()
        .is_ok_and(|s| s.current_state != ServiceState::Stopped)
    {
        let _ = service.stop();
        wait_for(&service, ServiceState::Stopped, Duration::from_secs(30));
    }
    drop(service);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        match manager.open_service(name, ServiceAccess::QUERY_STATUS) {
            Err(e) if code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
                return Ok("Removed the flux service. Its data stays.".into())
            }
            _ => std::thread::sleep(Duration::from_millis(250)),
        }
    }
    Ok(
        "The flux service is removed once Windows lets go of it (a restart at the latest). Its data stays."
            .into(),
    )
}
