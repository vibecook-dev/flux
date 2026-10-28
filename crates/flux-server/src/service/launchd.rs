//! macOS: flux as a LaunchAgent of the user who installs it (`service`).
//! launchd starts it at login in that user's session and again when it
//! exits with an error (never after a clean stop). After an update flux
//! starts the new version in its own place, so launchd keeps following it.

use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

/// The LaunchAgent's label for service `name`: `dev.vibecook.flux`.
pub fn label(name: &str) -> String {
    format!("dev.vibecook.{name}")
}

/// Where flux and Homebrew's ffmpeg usually are: a LaunchAgent starts with a
/// bare PATH (`/usr/bin:/bin:/usr/sbin:/sbin`).
pub const PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// The LaunchAgent's property list: `program` (flux-server and its
/// arguments) at login, again after an exit with an error (at most every 10
/// s), with its output in `log`. `Interactive`: without a process type
/// launchd throttles a job's CPU and I/O, and transcoding is real time.
pub fn plist(label: &str, program: &[String], log: &Path) -> String {
    let args: String = program
        .iter()
        .map(|a| format!("\t\t<string>{}</string>\n", xml(a)))
        .collect();
    let log = xml(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
{args}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ThrottleInterval</key>
	<integer>10</integer>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>{PATH}</string>
	</dict>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        label = xml(label),
    )
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// What `launchctl print` says of a loaded service: running, and its PID.
pub fn parse_print(out: &str) -> (bool, Option<u32>) {
    let mut running = false;
    let mut pid = None;
    for line in out.lines().map(str::trim) {
        if let Some(state) = line.strip_prefix("state = ") {
            running = state.trim() == "running";
        } else if let Some(p) = line.strip_prefix("pid = ") {
            pid = p.trim().parse().ok();
        }
    }
    (running, pid)
}

#[cfg(target_os = "macos")]
struct Paths {
    plist: PathBuf,
    log: PathBuf,
}

#[cfg(target_os = "macos")]
fn paths(name: &str) -> Result<Paths, String> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or("HOME isn't set")?;
    Ok(Paths {
        plist: home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", label(name))),
        log: home.join("Library/Logs/flux").join(format!("{name}.log")),
    })
}

#[cfg(target_os = "macos")]
pub fn run(command: super::ServiceCommand, args: &super::ServiceArgs) -> Result<String, String> {
    use super::ServiceCommand as C;
    use std::process::Command;

    let uid = uid();
    if uid == 0 {
        return Err(
            "run this as the user flux should run as, not as root: a LaunchAgent belongs to a user's \
                    session"
                .into(),
        );
    }
    let name = args.name.clone();
    let label = label(&name);
    let p = paths(&name)?;
    // The GUI session's domain; over SSH without a login on the Mac, the user's.
    let domain = |d: &str| format!("{d}/{uid}");
    let launchctl = |args: &[&str]| -> std::io::Result<std::process::Output> {
        Command::new("/bin/launchctl").args(args).output()
    };
    let loaded_in = || -> Option<(String, String)> {
        ["gui", "user"].into_iter().find_map(|d| {
            let target = format!("{}/{label}", domain(d));
            let out = launchctl(&["print", &target]).ok()?;
            out.status
                .success()
                .then(|| (target, String::from_utf8_lossy(&out.stdout).into_owned()))
        })
    };
    let bootstrap = || -> Result<(), String> {
        let mut last = String::new();
        for d in ["gui", "user"] {
            let out = launchctl(&["bootstrap", &domain(d), &p.plist.to_string_lossy()])
                .map_err(|e| e.to_string())?;
            if out.status.success() {
                return Ok(());
            }
            last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        }
        Err(format!("launchctl couldn't load {}: {last}", p.plist.display()))
    };
    match command {
        C::Install => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let program = super::command_line(&exe, args)?;
            for dir in [p.plist.parent(), p.log.parent()].into_iter().flatten() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            if let Some((target, _)) = loaded_in() {
                let _ = launchctl(&["bootout", &target]);
            }
            std::fs::write(&p.plist, plist(&label, &program, &p.log))
                .map_err(|e| format!("{}: {e}", p.plist.display()))?;
            bootstrap()?;
            if let Some((target, _)) = loaded_in() {
                let _ = launchctl(&["enable", &target]);
            }
            Ok(format!(
                "flux runs as a service of this Mac now, for this user: it starts at login, and again if it stops \
                 on its own.\n  {}\n  log: {}\n`flux-server service status` says how it's doing.",
                p.plist.display(),
                p.log.display()
            ))
        }
        C::Uninstall => {
            if let Some((target, _)) = loaded_in() {
                let _ = launchctl(&["bootout", &target]);
            }
            match std::fs::remove_file(&p.plist) {
                Ok(()) => Ok(format!(
                    "Removed the flux service ({}). Its data stays.",
                    p.plist.display()
                )),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Ok("No flux service was installed.".into())
                }
                Err(e) => Err(format!("{}: {e}", p.plist.display())),
            }
        }
        C::Start => {
            if !p.plist.exists() {
                return Err("the flux service isn't installed: `flux-server service install`".into());
            }
            match loaded_in() {
                Some((target, _)) => {
                    let out = launchctl(&["kickstart", &target]).map_err(|e| e.to_string())?;
                    if !out.status.success() {
                        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
                    }
                }
                None => bootstrap()?,
            }
            Ok("flux is starting.".into())
        }
        C::Stop => match loaded_in() {
            Some((target, _)) => {
                // A clean exit, so launchd doesn't start it again (until the next login).
                let _ = launchctl(&["kill", "SIGTERM", &target]);
                Ok("flux is stopping; it starts again at the next login, or with `flux-server service start`.".into())
            }
            None => Ok("The flux service isn't running.".into()),
        },
        C::Status => {
            if !p.plist.exists() {
                return Ok("The flux service isn't installed (`flux-server service install`).".into());
            }
            let state = match loaded_in() {
                Some((_, out)) => match parse_print(&out) {
                    (true, Some(pid)) => format!("running (pid {pid})"),
                    (true, None) => "running".into(),
                    (false, _) => "stopped".into(),
                },
                None => {
                    "not loaded (it starts at the next login, or with `flux-server service start`)".into()
                }
            };
            Ok(format!(
                "flux service: {state}\n  {}\n  log: {}",
                p.plist.display(),
                p.log.display()
            ))
        }
        C::Run => Err("`service run` is for Windows".into()),
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn uid() -> u32 {
    // SAFETY: getuid takes nothing and can't fail.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_runs_flux_at_login_and_after_a_failure() {
        let program = vec![
            "/Users/me/flux/bin/flux-server".to_string(),
            "--config".into(),
            "/Users/me/Films & TV/flux.toml".into(),
        ];
        let p = plist(
            &label("flux"),
            &program,
            Path::new("/Users/me/Library/Logs/flux/flux.log"),
        );
        assert!(p.contains("<string>dev.vibecook.flux</string>"), "{p}");
        assert!(
            p.contains("<string>/Users/me/Films &amp; TV/flux.toml</string>"),
            "escaped: {p}"
        );
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"), "{p}");
        // Not after a clean exit: `service stop`, or flux stopping on Ctrl-C.
        assert!(p.contains("<key>SuccessfulExit</key>\n\t\t<false/>"), "{p}");
        assert!(p.contains("/opt/homebrew/bin"), "Homebrew's ffmpeg: {p}");
        assert!(
            p.contains("<key>ProcessType</key>\n\t<string>Interactive</string>"),
            "not throttled: {p}"
        );
        let order: Vec<usize> = program.iter().map(|a| p.find(&xml(a)).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "arguments in order: {p}");
    }

    #[test]
    fn reads_launchctl_print() {
        let out = "gui/501/dev.vibecook.flux = {\n\tactive count = 1\n\tpath = /Users/me/Library/LaunchAgents/dev.vibecook.flux.plist\n\tstate = running\n\n\tprogram = /x/flux-server\n\tpid = 4242\n}";
        assert_eq!(parse_print(out), (true, Some(4242)));
        assert_eq!(parse_print("\tstate = not running\n"), (false, None));
    }
}
