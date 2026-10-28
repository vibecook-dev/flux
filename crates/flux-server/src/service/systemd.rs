//! Linux: flux as a systemd user unit of the user who installs it
//! (`service`). systemd starts it at login (at boot, once `loginctl
//! enable-linger` is on for the user) and again when it exits with an error,
//! never after a clean stop. After an update flux starts the new version in
//! its own place, so systemd keeps following it.

/// The unit for `exec` (flux-server and its arguments).
pub fn unit(exec: &[String]) -> String {
    let exec: Vec<String> = exec.iter().map(|a| quote(a)).collect();
    format!(
        "[Unit]
Description=flux media server
Documentation=https://github.com/vibecook-dev/flux/blob/main/docs/INSTALL.md
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={}
# A clean stop is the end; an error (or exit 75, flux asking to be started
# again when it couldn't do that itself) brings it back.
Restart=on-failure
RestartSec=5
TimeoutStopSec=20
Environment=NO_COLOR=1

[Install]
WantedBy=default.target
",
        exec.join(" ")
    )
}

/// One `ExecStart` argument, quoted as systemd reads it: in double quotes,
/// with `\\` and `\"` escaped, and `%` and `$` doubled (specifiers and
/// variables).
fn quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What `systemctl show` says, as `key=value` lines.
pub fn parse_show(out: &str) -> std::collections::HashMap<&str, &str> {
    out.lines().filter_map(|l| l.split_once('=')).collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn run(command: super::ServiceCommand, args: &super::ServiceArgs) -> Result<String, String> {
    use super::ServiceCommand as C;
    use std::path::PathBuf;
    use std::process::Command;

    if is_root() {
        return Err("run this as the user flux should run as, not as root: it installs a systemd user unit. For \
                    a system service, install the .deb or .rpm, or write a system unit (docs/INSTALL.md, As a \
                    service)."
            .into());
    }
    let name = args.name.clone();
    let unit_name = format!("{name}.service");
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or("HOME isn't set")?;
    let path = config_home.join("systemd/user").join(&unit_name);
    let systemctl = |args: &[&str]| -> Result<String, String> {
        let out = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .map_err(|e| format!("can't run systemctl ({e}): this needs systemd"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "systemctl --user {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    };
    let show = || {
        systemctl(&[
            "show",
            &unit_name,
            "--property=LoadState,ActiveState,SubState,MainPID",
        ])
    };
    match command {
        C::Install => {
            // A user session's systemd must be there (not in every container or SSH session).
            systemctl(&["show-environment"]).map_err(|e| {
                format!("{e}\nsystemd's user manager doesn't answer here (a container, or an SSH session without a login?).")
            })?;
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let exec = super::command_line(&exe, args)?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            std::fs::write(&path, unit(&exec)).map_err(|e| format!("{}: {e}", path.display()))?;
            systemctl(&["daemon-reload"])?;
            systemctl(&["enable", "--now", &unit_name])?;
            let user = std::env::var("USER").unwrap_or_default();
            let lingering = Command::new("loginctl")
                .args(["show-user", &user, "--property=Linger", "--value"])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes");
            let linger = if lingering {
                String::new()
            } else {
                "\nIt starts when you log in. To start it at boot instead, without a login: loginctl enable-linger"
                    .to_string()
            };
            Ok(format!(
                "flux runs as a service of this user now, and again if it stops on its own.\n  {}\n  log: journalctl --user -u {name}{linger}",
                path.display()
            ))
        }
        C::Uninstall => {
            let _ = systemctl(&["disable", "--now", &unit_name]);
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    let _ = systemctl(&["daemon-reload"]);
                    Ok(format!("Removed the flux service ({}). Its data stays.", path.display()))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("No flux service was installed.".into()),
                Err(e) => Err(format!("{}: {e}", path.display())),
            }
        }
        C::Start => systemctl(&["start", &unit_name]).map(|_| "flux is starting.".into()),
        C::Stop => systemctl(&["stop", &unit_name])
            .map(|_| "flux stopped; it starts again at the next login or boot, or with `flux-server service start`.".into()),
        C::Status => {
            if !path.exists() {
                return Ok("The flux service isn't installed (`flux-server service install`).".into());
            }
            let out = show()?;
            let v = parse_show(&out);
            let state = match (v.get("ActiveState").copied(), v.get("MainPID").copied()) {
                (Some("active"), Some(pid)) if pid != "0" => format!("running (pid {pid})"),
                (Some(a), _) => format!("{a} ({})", v.get("SubState").copied().unwrap_or("?")),
                (None, _) => "unknown".into(),
            };
            Ok(format!("flux service: {state}\n  {}\n  log: journalctl --user -u {name}", path.display()))
        }
        C::Run => Err("`service run` is for Windows".into()),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
#[allow(unsafe_code)]
fn is_root() -> bool {
    // SAFETY: geteuid takes nothing and can't fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_restarts_flux_after_an_error_only() {
        let u = unit(&[
            "/opt/flux/bin/flux-server".into(),
            "--config".into(),
            "/home/me/my \"films\"/100%/$HOME.toml".into(),
        ]);
        assert!(
            u.contains(r#"ExecStart="/opt/flux/bin/flux-server" "--config" "/home/me/my \"films\"/100%%/$$HOME.toml""#),
            "{u}"
        );
        assert!(u.contains("Restart=on-failure"), "{u}");
        assert!(u.contains("WantedBy=default.target"), "{u}");
    }

    #[test]
    fn reads_systemctl_show() {
        let v = parse_show("LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=4242\n");
        assert_eq!((v["ActiveState"], v["MainPID"]), ("active", "4242"));
    }
}
