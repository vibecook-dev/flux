//! Where things are: inside flux.app, and in the home folder.

use std::path::{Path, PathBuf};

/// The flux.app this program runs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// `…/flux.app`.
    pub root: PathBuf,
}

impl Bundle {
    /// The bundle of the app's own executable, `flux.app/Contents/MacOS/flux`
    /// (its real path).
    pub fn of_exe(exe: &Path) -> Option<Bundle> {
        let macos = exe.parent()?;
        let contents = macos.parent()?;
        let root = contents.parent()?;
        let named = |p: &Path, n: &str| p.file_name().is_some_and(|f| f == n);
        (named(macos, "MacOS") && named(contents, "Contents") && root.extension().is_some_and(|e| e == "app"))
            .then(|| Bundle {
                root: root.to_path_buf(),
            })
    }

    /// The programs beside flux.app's own: flux-server, flux, the sidecar
    /// and FFmpeg.
    pub fn helpers(&self) -> PathBuf {
        self.root.join("Contents/Helpers")
    }

    pub fn server(&self) -> PathBuf {
        self.helpers().join("flux-server")
    }

    pub fn resource(&self, name: &str) -> PathBuf {
        self.root.join("Contents/Resources").join(name)
    }

    /// Whether it runs from somewhere it can't stay: its disk image, or a
    /// read-only copy macOS made of it (App Translocation, for an app opened
    /// where it was downloaded). It can't update itself there.
    pub fn needs_moving(&self) -> bool {
        let p = self.root.to_string_lossy();
        p.contains("/AppTranslocation/") || p.starts_with("/Volumes/")
    }
}

/// `~/Library/Application Support/flux`: flux-server's default data dir on
/// macOS, which the app leaves it to use.
pub fn data_dir(home: &Path) -> PathBuf {
    home.join("Library/Application Support/flux")
}

/// `~/Library/Logs/flux/flux.log`, where `flux-server service install`'s
/// LaunchAgent logs too.
pub fn log_file(home: &Path) -> PathBuf {
    home.join("Library/Logs/flux/flux.log")
}

/// The log is started afresh past this size, the old one kept as
/// `flux.log.1`.
pub const LOG_LIMIT: u64 = 20 * 1024 * 1024;

/// Keeps the log under `limit`: a longer one becomes `<log>.1`, replacing
/// the one before.
pub fn rotate_log(log: &Path, limit: u64) -> std::io::Result<()> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::metadata(log) {
        Ok(m) if m.len() > limit => std::fs::rename(log, log.with_extension("log.1")),
        _ => Ok(()),
    }
}

/// Copies the settings file's template (all comments) into `data_dir` as
/// `flux.toml`, unless one is there: true when it did.
pub fn write_settings(template: &Path, data_dir: &Path) -> std::io::Result<bool> {
    let dest = data_dir.join("flux.toml");
    if dest.symlink_metadata().is_ok() {
        return Ok(false);
    }
    std::fs::create_dir_all(data_dir)?;
    std::fs::copy(template, &dest)?;
    Ok(true)
}

/// `s` in single quotes for `/bin/sh`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `s` as an AppleScript string literal.
pub fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

/// Where the command line tools are linked: `/usr/local/bin`, on the PATH
/// of every macOS shell.
pub const TOOLS_DIR: &str = "/usr/local/bin";

/// The command line tools linked from [`TOOLS_DIR`]: `flux` (for people and
/// their agents) and `flux-server` (`owner-link`, `--check`, `update`).
pub const TOOLS: [&str; 2] = ["flux", "flux-server"];

/// How a tool in [`TOOLS_DIR`] stands: missing, flux.app's (any copy's), or
/// something else (Homebrew's flux, a release package's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Missing,
    App,
    Other,
}

pub fn tool_at(link: &Path) -> Tool {
    match std::fs::read_link(link) {
        Ok(to) if to.to_string_lossy().contains(".app/Contents/Helpers/") => Tool::App,
        Ok(_) => Tool::Other,
        Err(_) if link.symlink_metadata().is_ok() => Tool::Other,
        Err(_) => Tool::Missing,
    }
}

/// The shell command that links the tools of `bundle` from `dir`.
pub fn link_tools_command(bundle: &Bundle, dir: &str) -> String {
    let mut cmd = format!("mkdir -p {}", shell_quote(dir));
    for t in TOOLS {
        cmd.push_str(&format!(
            " && ln -sfn {} {}",
            shell_quote(&bundle.helpers().join(t).to_string_lossy()),
            shell_quote(&format!("{dir}/{t}"))
        ));
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_its_bundle_from_its_executable() {
        let b = Bundle::of_exe(Path::new("/Applications/flux.app/Contents/MacOS/flux")).unwrap();
        assert_eq!(b.root, Path::new("/Applications/flux.app"));
        assert_eq!(
            b.server(),
            Path::new("/Applications/flux.app/Contents/Helpers/flux-server")
        );
        assert_eq!(
            b.resource("flux.toml"),
            Path::new("/Applications/flux.app/Contents/Resources/flux.toml")
        );
        assert!(!b.needs_moving());
        assert_eq!(Bundle::of_exe(Path::new("/opt/flux/bin/flux")), None);
        assert_eq!(Bundle::of_exe(Path::new("/x/flux/Contents/MacOS/flux")), None);
    }

    #[test]
    fn knows_when_it_runs_from_its_disk_image() {
        for p in [
            "/Volumes/flux 0.1.0/flux.app/Contents/MacOS/flux",
            "/private/var/folders/ab/T/AppTranslocation/4F2A/d/flux.app/Contents/MacOS/flux",
        ] {
            assert!(Bundle::of_exe(Path::new(p)).unwrap().needs_moving(), "{p}");
        }
    }

    #[test]
    fn rotates_a_long_log() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("Logs/flux/flux.log");
        rotate_log(&log, 10).unwrap();
        assert!(log.parent().unwrap().is_dir(), "the folder is made");
        std::fs::write(&log, "short").unwrap();
        rotate_log(&log, 10).unwrap();
        assert!(log.is_file());
        std::fs::write(&log, "much longer than ten bytes").unwrap();
        rotate_log(&log, 10).unwrap();
        assert!(!log.exists());
        assert!(d.path().join("Logs/flux/flux.log.1").is_file());
    }

    #[test]
    fn writes_the_settings_once() {
        let d = tempfile::tempdir().unwrap();
        let template = d.path().join("flux.toml");
        std::fs::write(&template, "# settings\n").unwrap();
        let data = d.path().join("Application Support/flux");
        assert!(write_settings(&template, &data).unwrap());
        std::fs::write(data.join("flux.toml"), "lan = \"0.0.0.0:8097\"\n").unwrap();
        assert!(!write_settings(&template, &data).unwrap());
        assert_eq!(
            std::fs::read_to_string(data.join("flux.toml")).unwrap(),
            "lan = \"0.0.0.0:8097\"\n"
        );
    }

    #[test]
    fn quotes_for_the_shell_and_applescript() {
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
        assert_eq!(
            applescript_quote(r#"say "hi" \ there"#),
            r#""say \"hi\" \\ there""#
        );
        let b = Bundle {
            root: PathBuf::from("/Applications/flux.app"),
        };
        assert_eq!(
            link_tools_command(&b, "/usr/local/bin"),
            "mkdir -p '/usr/local/bin' \
             && ln -sfn '/Applications/flux.app/Contents/Helpers/flux' '/usr/local/bin/flux' \
             && ln -sfn '/Applications/flux.app/Contents/Helpers/flux-server' '/usr/local/bin/flux-server'"
        );
    }

    #[test]
    fn tells_its_tools_from_others() {
        let d = tempfile::tempdir().unwrap();
        let link = d.path().join("flux");
        assert_eq!(tool_at(&link), Tool::Missing);
        std::os::unix::fs::symlink("/Applications/flux.app/Contents/Helpers/flux", &link).unwrap();
        assert_eq!(tool_at(&link), Tool::App);
        let brew = d.path().join("flux-server");
        std::os::unix::fs::symlink("../Cellar/flux/0.1.0/bin/flux-server", &brew).unwrap();
        assert_eq!(tool_at(&brew), Tool::Other);
        let file = d.path().join("plain");
        std::fs::write(&file, "").unwrap();
        assert_eq!(tool_at(&file), Tool::Other);
    }
}
