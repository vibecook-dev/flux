//! The Windows installer's flux updates with the next installer
//! (docs/INSTALL.md § Updating). flux downloads the release's
//! `flux-<version>-x86_64-pc-windows-msvc.msi` into `updates` in the data
//! dir, a folder for SYSTEM and Administrators only, checks it against
//! `SHA256SUMS`, and hands it to a copy of itself there ([`RUNNER`]):
//! nothing may run from the program folder while Windows Installer replaces
//! it. The copy leaves flux's job object (ffmpeg stays in it, so it ends
//! with flux) and runs `msiexec`, which stops the service, puts the new
//! version in place and starts the service again; a failed install puts the
//! old version back, and the copy makes sure the service runs. It writes
//! what happened to [`OUTCOME`], which flux reads when it starts again (or
//! while it waits, [`PENDING`]), so the update panel can say why an update
//! failed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// In `updates`: the copy of flux that runs the installer.
pub const RUNNER: &str = "flux-update.exe";
/// In `updates`: what the installer did (an [`Outcome`]).
pub const OUTCOME: &str = "msi-result.json";
/// In `updates`: an update handed to the installer (a [`Pending`]).
pub const PENDING: &str = "msi-pending.json";

/// An update handed to the installer, not known to be finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub version: String,
    /// Unix epoch milliseconds.
    pub started_at: i64,
}

/// What `msiexec` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// The version it installed, or tried to.
    pub version: String,
    /// Its exit code.
    pub code: i32,
    /// Its log (`/l*v`).
    pub log: PathBuf,
    /// Unix epoch milliseconds.
    pub finished_at: i64,
}

/// `msiexec`'s exit codes that mean the new version is installed (with 1641
/// and 3010, a restart of Windows finishes it).
pub fn succeeded(code: i32) -> bool {
    matches!(code, 0 | 1641 | 3010)
}

/// Another installation is under way: try again in a moment.
pub const BUSY: i32 = 1618;

/// What an exit code of `msiexec` means, for a person.
pub fn meaning(code: i32) -> &'static str {
    match code {
        0 => "done",
        1602 => "cancelled",
        1603 => "a fatal error during installation",
        BUSY => "another installation kept going",
        1619 | 1620 => "Windows Installer couldn't open the package",
        1625 | 1643 | 1644 => "a system policy doesn't allow it",
        1638 => "another version of flux is installed",
        1641 | 3010 => "done; restart Windows to finish",
        _ => "Windows Installer failed",
    }
}

/// What to tell the owner about an installer run that left `current` in
/// place: nothing when it worked.
pub fn failure(outcome: &Outcome, current: &str) -> Option<String> {
    if succeeded(outcome.code) || outcome.version == current {
        return None;
    }
    Some(format!(
        "Windows Installer couldn't install flux {}: {} (exit code {}). Its log: {}",
        outcome.version,
        meaning(outcome.code),
        outcome.code,
        outcome.log.display()
    ))
}

/// The installer run recorded in `dir` (`updates`), taken: read and removed
/// with its pending mark.
pub fn take_outcome(dir: &Path) -> Option<Outcome> {
    let path = dir.join(OUTCOME);
    let outcome = serde_json::from_slice(&std::fs::read(&path).ok()?).ok();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(dir.join(PENDING));
    outcome
}

/// Removes the installers and the runner from `dir` once their outcome is
/// in; the logs stay. (The runner may still be ending: it goes next time.)
pub fn tidy(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".msi") || name == RUNNER {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The update handed to the installer from `dir`, if one is.
pub fn pending(dir: &Path) -> Option<Pending> {
    serde_json::from_slice(&std::fs::read(dir.join(PENDING)).ok()?).ok()
}

/// Writes `value` as JSON to `path`, whole or not at all.
pub fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let tmp = path.with_extension("json.part");
    std::fs::write(&tmp, serde_json::to_vec(value).map_err(std::io::Error::other)?)?;
    std::fs::rename(&tmp, path)
}

#[cfg(windows)]
pub use windows::{hand_over, run_installer};

#[cfg(windows)]
mod windows {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use anyhow::Context;
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
    };

    use super::{succeeded, write_json, Outcome, Pending, BUSY, OUTCOME, PENDING, RUNNER};

    /// Starts [`RUNNER`] (a copy of this flux in `msi`'s folder) on
    /// installer `msi` of `version`, and returns: the runner goes on when
    /// this process ends. `after`: a process the runner waits for first
    /// (`flux-server update` in a terminal, which runs from the program
    /// folder).
    pub fn hand_over(msi: &Path, version: &str, data_dir: &Path, after: Option<u32>) -> anyhow::Result<()> {
        let dir = msi.parent().context("the installer has no folder")?;
        let exe = std::env::current_exe().context("finding flux-server.exe")?;
        let runner = dir.join(RUNNER);
        std::fs::copy(&exe, &runner).with_context(|| format!("copying flux to {}", runner.display()))?;
        write_json(
            &dir.join(PENDING),
            &Pending {
                version: version.to_string(),
                started_at: super::super::now_ms(),
            },
        )
        .context("writing the update's mark")?;
        let mut cmd = Command::new(&runner);
        cmd.arg("--data-dir")
            .arg(data_dir)
            .args(["update", "--install-msi"])
            .arg(msi)
            .args(["--to", version]);
        if let Some(pid) = after {
            cmd.args(["--after", &pid.to_string()]);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Out of flux's job, where ffmpeg stays: Windows ends the job's
        // processes with flux, and the installer stops flux.
        if let Err(e) = crate::win32::let_children_break_away() {
            tracing::warn!(error = %e, "can't let the installer's runner leave flux's job");
        }
        let detached = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        match cmd.creation_flags(detached | CREATE_BREAKAWAY_FROM_JOB).spawn() {
            Ok(_) => Ok(()),
            // A job above flux's that doesn't allow it: stay in the job, and
            // keep its processes alive when flux ends.
            Err(e) => {
                tracing::warn!(error = %e, "the installer's runner can't leave flux's job; it stays in it");
                crate::win32::let_children_outlive_this_process()
                    .context("letting the runner outlive flux")?;
                cmd.creation_flags(detached)
                    .spawn()
                    .with_context(|| format!("starting {}", runner.display()))?;
                Ok(())
            }
        }
    }

    /// The runner: `flux-update.exe update --install-msi <msi> --to
    /// <version>`. Runs Windows Installer on `msi` (again while another
    /// installation is under way), records what it did, and makes sure
    /// the service runs when the install failed.
    pub fn run_installer(
        data_dir: &Path,
        msi: &Path,
        version: &str,
        service: &str,
        after: Option<u32>,
    ) -> u8 {
        if let Some(pid) = after {
            crate::win32::wait_for_exit(pid, Duration::from_secs(60));
        }
        let dir = msi
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| data_dir.to_path_buf());
        let log = dir.join(format!("msi-{version}.log"));
        let msiexec = std::env::var_os("SystemRoot")
            .map(|r| PathBuf::from(r).join("System32").join("msiexec.exe"))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("msiexec.exe"));
        let mut code = -1;
        for attempt in 0..40 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(15));
            }
            code = match Command::new(&msiexec)
                .arg("/i")
                .arg(msi)
                .args(["/qn", "/norestart", "/l*v"])
                .arg(&log)
                .status()
            {
                Ok(status) => status.code().unwrap_or(-1),
                Err(_) => -1,
            };
            if code != BUSY {
                break;
            }
        }
        let outcome = Outcome {
            version: version.to_string(),
            code,
            log,
            finished_at: super::super::now_ms(),
        };
        let _ = write_json(&dir.join(OUTCOME), &outcome);
        if succeeded(code) {
            return 0;
        }
        // Windows Installer puts the old version back; start it if the
        // service didn't come back with it.
        let _ = ensure_running(service);
        1
    }

    fn ensure_running(name: &str) -> windows_service::Result<()> {
        use windows_service::service::{ServiceAccess, ServiceState};
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::START)?;
        if service.query_status()?.current_state == ServiceState::Stopped {
            service.start::<&std::ffi::OsStr>(&[])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(version: &str, code: i32) -> Outcome {
        Outcome {
            version: version.into(),
            code,
            log: PathBuf::from(r"C:\ProgramData\flux\updates\msi-0.2.0.log"),
            finished_at: 0,
        }
    }

    #[test]
    fn says_why_an_install_failed() {
        assert_eq!(failure(&outcome("0.2.0", 0), "0.2.0"), None);
        assert_eq!(failure(&outcome("0.2.0", 3010), "0.1.0"), None);
        let why = failure(&outcome("0.2.0", 1603), "0.1.0").unwrap();
        assert!(why.contains("flux 0.2.0"), "{why}");
        assert!(why.contains("a fatal error"), "{why}");
        assert!(why.contains("1603"), "{why}");
        assert!(why.contains("msi-0.2.0.log"), "{why}");
        // The version it tried is running: nothing to say.
        assert_eq!(failure(&outcome("0.2.0", 1603), "0.2.0"), None);
    }

    #[test]
    fn takes_the_outcome_once() {
        let dir = std::env::temp_dir().join(format!("flux-msi-outcome-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_json(
            &dir.join(PENDING),
            &Pending {
                version: "0.2.0".into(),
                started_at: 1,
            },
        )
        .unwrap();
        assert_eq!(pending(&dir).map(|p| p.version), Some("0.2.0".into()));
        assert_eq!(take_outcome(&dir), None);
        write_json(&dir.join(OUTCOME), &outcome("0.2.0", 1618)).unwrap();
        std::fs::write(dir.join("flux-0.2.0-x86_64-pc-windows-msvc.msi"), "msi").unwrap();
        std::fs::write(dir.join("msi-0.2.0.log"), "log").unwrap();
        let got = take_outcome(&dir).unwrap();
        tidy(&dir);
        assert!(!dir.join("flux-0.2.0-x86_64-pc-windows-msvc.msi").exists());
        assert!(dir.join("msi-0.2.0.log").exists());
        assert_eq!(got.code, 1618);
        assert_eq!(meaning(got.code), "another installation kept going");
        assert_eq!(take_outcome(&dir), None);
        assert_eq!(pending(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
