//! `flux-server service`: flux as a service of the machine it runs on, so it
//! starts by itself and comes back when it stops on its own:
//!
//! - **macOS**: a LaunchAgent for the user who installs it
//!   (`~/Library/LaunchAgents/dev.vibecook.flux.plist`). launchd runs it at
//!   login, in that user's session, so flux reads that user's folders and
//!   volumes as it does when started in a terminal. Log:
//!   `~/Library/Logs/flux/flux.log`.
//! - **Linux**: a systemd user unit (`~/.config/systemd/user/flux.service`),
//!   started at login, or at boot once `loginctl enable-linger` is on. Log:
//!   `journalctl --user -u flux`.
//! - **Windows**: a Windows service named `flux`, run as LocalSystem, that
//!   starts with Windows and that the service manager starts again when it
//!   stops on its own (after an update, too). It runs `flux-server service
//!   run`. Log: `flux.log` in the data dir (`C:\ProgramData\flux` unless
//!   `--data-dir` or the config file says otherwise).
//!
//! The service runs flux with the flags given to `service install` that say
//! where its config, data and address are (`--config`, `--data-dir`,
//! `--port`, `--bind`); anything else goes in the config file. An install
//! that comes with a service of its own (Homebrew, the .deb and .rpm, the
//! Windows installer, Docker) says how to use that one instead.

use std::ffi::OsString;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use flux_core::api::InstallKind;

use crate::config::Config;

pub mod launchd;
#[cfg(windows)]
pub mod scm;
pub mod systemd;

/// `flux-server service …`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub enum ServiceCommand {
    /// Install flux as a service of this machine and start it: on macOS for
    /// this user, at login; on Linux a systemd user unit; on Windows a
    /// Windows service (run it as administrator). The service runs flux with
    /// this command's --config, --data-dir, --port and --bind.
    Install,
    /// Stop the service and remove it. flux's data stays.
    Uninstall,
    /// Start the service.
    Start,
    /// Stop the service, until it is started again (or the next login or
    /// boot starts it).
    Stop,
    /// Whether the service is installed and running, and where it logs.
    Status,
    /// What the Windows service manager runs: flux as a Windows service.
    #[command(hide = true)]
    Run,
}

/// The service's name unless `--service-name` gives another.
pub const NAME: &str = "flux";

/// The flags a service runs flux with: the ones given to `service install`
/// that say where flux's config, data and address are, paths made absolute
/// (a service doesn't start in the folder the command ran in), and the
/// service's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceArgs {
    /// `flux`, or another for tests (`--service-name`), so they don't touch
    /// a real install's service.
    pub name: String,
    pub config: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub port: Option<u16>,
    pub bind: Option<IpAddr>,
}

impl ServiceArgs {
    pub fn of(cli: &crate::Cli) -> ServiceArgs {
        let absolute = |p: &PathBuf| std::path::absolute(p).unwrap_or_else(|_| p.clone());
        let name = cli
            .service_name
            .clone()
            .filter(|n| {
                !n.is_empty() && n.len() <= 40 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
            .unwrap_or_else(|| NAME.to_string());
        ServiceArgs {
            name,
            config: cli.config.as_ref().map(absolute),
            data_dir: cli.data_dir.as_ref().map(absolute),
            port: cli.port,
            bind: cli.bind,
        }
    }

    /// As arguments after flux-server's own path.
    pub fn args(&self) -> Vec<OsString> {
        let mut out: Vec<OsString> = Vec::new();
        if self.name != NAME {
            out.extend(["--service-name".into(), self.name.clone().into()]);
        }
        if let Some(c) = &self.config {
            out.extend(["--config".into(), c.clone().into_os_string()]);
        }
        if let Some(d) = &self.data_dir {
            out.extend(["--data-dir".into(), d.clone().into_os_string()]);
        }
        if let Some(p) = self.port {
            out.extend(["--port".into(), p.to_string().into()]);
        }
        if let Some(b) = self.bind {
            out.extend(["--bind".into(), b.to_string().into()]);
        }
        out
    }
}

/// For installs whose package brings a service of its own: how to use it.
pub fn own_service(kind: InstallKind) -> Option<&'static str> {
    match kind {
        InstallKind::Homebrew => Some(
            "flux from Homebrew runs with Homebrew's services: `brew services start flux` (and `brew services \
             stop flux`).",
        ),
        InstallKind::Deb | InstallKind::Rpm => Some(
            "The package installed flux as a system service: `sudo systemctl enable --now flux` starts it now \
             and at every boot (its log: `journalctl -u flux`).",
        ),
        InstallKind::Msi => Some(
            "The installer set flux up as a Windows service already: it starts with Windows. Start or stop it \
             in Services (flux), or with `sc start flux` and `sc stop flux` as administrator.",
        ),
        InstallKind::Docker => Some(
            "In Docker the container is the service: run it with `--restart unless-stopped` (in Compose, \
             `restart: unless-stopped`).",
        ),
        InstallKind::Archive | InstallKind::Source => None,
    }
}

/// `flux-server service <command>` (all but `run`, which `main` starts).
pub fn cli(command: ServiceCommand, args: &ServiceArgs, cfg: &Config) -> ExitCode {
    if command == ServiceCommand::Install {
        if let Some(say) = own_service(crate::update::install::Install::detect().kind) {
            eprintln!("{say}");
            return ExitCode::from(2);
        }
    }
    match platform(command, args, cfg) {
        Ok(said) => {
            println!("{said}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("flux-server service: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
fn platform(command: ServiceCommand, args: &ServiceArgs, _cfg: &Config) -> Result<String, String> {
    launchd::run(command, args)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform(command: ServiceCommand, args: &ServiceArgs, _cfg: &Config) -> Result<String, String> {
    systemd::run(command, args)
}

#[cfg(windows)]
fn platform(command: ServiceCommand, args: &ServiceArgs, cfg: &Config) -> Result<String, String> {
    scm::run(command, args, cfg)
}

#[cfg(not(any(unix, windows)))]
fn platform(_: ServiceCommand, _: &ServiceArgs, _: &Config) -> Result<String, String> {
    Err("services aren't supported on this system".into())
}

/// The program and its arguments, as text, for the files launchd and systemd
/// read (they hold paths as UTF-8).
#[cfg(unix)]
pub(crate) fn command_line(exe: &std::path::Path, args: &ServiceArgs) -> Result<Vec<String>, String> {
    std::iter::once(exe.as_os_str().to_os_string())
        .chain(args.args())
        .map(|a| {
            a.into_string().map_err(|a| {
                format!(
                    "{} isn't valid UTF-8, which a service file can't hold",
                    a.to_string_lossy()
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn a_service_runs_with_the_flags_that_say_where_flux_is() {
        let cli = crate::Cli::try_parse_from([
            "flux-server",
            "--config",
            "flux.toml",
            "--port",
            "9000",
            "--lan",
            "0.0.0.0:9001",
            "service",
            "install",
        ])
        .unwrap();
        let args = ServiceArgs::of(&cli);
        let config = args.config.clone().expect("the config");
        assert!(
            config.is_absolute() && config.ends_with("flux.toml"),
            "{config:?}"
        );
        assert_eq!(
            (args.data_dir.as_ref(), args.port, args.bind),
            (None, Some(9000), None)
        );
        // --lan isn't one of them: it belongs in the config file.
        let line: Vec<String> = args
            .args()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(line, ["--config", config.to_str().unwrap(), "--port", "9000"]);
        assert!(matches!(
            cli.command,
            Some(crate::config::Command::Service {
                action: ServiceCommand::Install
            })
        ));
    }

    #[test]
    fn packaged_installs_use_their_own_service() {
        for kind in [
            InstallKind::Homebrew,
            InstallKind::Deb,
            InstallKind::Rpm,
            InstallKind::Msi,
            InstallKind::Docker,
        ] {
            assert!(own_service(kind).is_some(), "{kind:?}");
        }
        assert!(own_service(InstallKind::Archive).is_none());
        assert!(own_service(InstallKind::Source).is_none());
    }

    #[test]
    fn the_name_is_flux_unless_a_test_picks_another() {
        let named = |args: &[&str]| ServiceArgs::of(&crate::Cli::try_parse_from(args).unwrap());
        if std::env::var_os("FLUX_SERVICE_NAME").is_none() {
            assert_eq!(named(&["flux-server", "service", "status"]).name, "flux");
        }
        let test = named(&[
            "flux-server",
            "service",
            "install",
            "--service-name",
            "flux-test-7",
        ]);
        assert_eq!(test.name, "flux-test-7");
        // The service runs with its name, so it knows itself.
        assert_eq!(test.args(), ["--service-name", "flux-test-7"]);
        // Not a name a service file could be called.
        assert_eq!(
            named(&["flux-server", "--service-name", "../x", "service", "status"]).name,
            "flux"
        );
    }
}
