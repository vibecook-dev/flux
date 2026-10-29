//! The macOS app's updates: flux.app replaced as a whole by the one in the
//! release's disk image.
//!
//! flux downloads `flux-<version>-universal-apple-darwin.dmg`, checks it
//! against `SHA256SUMS`, mounts it (read-only, out of Finder's sight) and
//! copies its flux.app beside the installed one, as `.flux-update.app` on
//! the same volume. The copy must be signed by the same team as the running
//! app (when that one is signed), and its flux-server must start and say the
//! new version. Then it is renamed into place: the installed bundle steps
//! aside as `.flux-replaced-<pid>.app`, the new one takes its name, and the
//! old one is deleted (the running processes keep their files open). Last,
//! flux restarts: under the app, flux-server exits for the app to start
//! itself again from the new bundle (`main.rs`, `FLUX_RESTART=exit`).
//!
//! Nothing inside a signed bundle changes, so nothing is kept for
//! `--rollback`: the release page has every version's disk image.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// The new bundle while it is checked, beside the installed one.
pub const STAGED: &str = ".flux-update.app";

/// Whether flux can replace the bundle at `app`: why not, for a person, if
/// it can't.
pub fn why_not_replace(app: &Path) -> Option<String> {
    let path = app.to_string_lossy();
    if path.contains("/AppTranslocation/") || path.starts_with("/Volumes/") {
        return Some(
            "flux is running from its disk image or the Downloads folder, so it can't update itself: \
             move flux to Applications (its menu offers to), then open it from there."
                .into(),
        );
    }
    let parent = app.parent()?;
    if !super::install::writable(parent) || !super::install::writable(app) {
        return Some(format!(
            "flux can't replace itself in {}: install the new version from its disk image, or ask \
             an administrator of this Mac to.",
            parent.display()
        ));
    }
    None
}

/// The team that signed the code at `path`, from `codesign -dv`'s
/// `TeamIdentifier=` line: `None` when it isn't signed, or is signed without
/// a team (ad hoc, as in a build without the Apple secrets).
pub fn team_of(codesign_output: &str) -> Option<String> {
    codesign_output
        .lines()
        .find_map(|l| l.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != "not set")
        .map(str::to_string)
}

/// Puts `staged` (a complete new bundle beside `app`) in `app`'s place: `app`
/// steps aside as `aside`, `staged` takes its name, and `aside` is deleted.
/// If the second rename fails, `app` goes back.
pub fn swap(app: &Path, staged: &Path, aside: &Path) -> anyhow::Result<()> {
    if aside.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(aside).with_context(|| format!("clearing {}", aside.display()))?;
    }
    std::fs::rename(app, aside).with_context(|| format!("moving {} aside", app.display()))?;
    if let Err(e) = std::fs::rename(staged, app) {
        let back = std::fs::rename(aside, app);
        return Err(anyhow::Error::new(e).context(match back {
            Ok(()) => format!("putting the new flux.app in {}", app.display()),
            Err(b) => format!(
                "putting the new flux.app in {} (and moving the old one back failed too: {b}; it is \
                 at {})",
                app.display(),
                aside.display()
            ),
        }));
    }
    if let Err(e) = std::fs::remove_dir_all(aside) {
        tracing::warn!(error = %e, "couldn't delete the replaced {}", aside.display());
    }
    Ok(())
}

/// Installs the flux.app in disk image `dmg` over `app`; `mount` is an empty
/// folder to mount it at. The new flux-server must say `version`.
#[cfg(target_os = "macos")]
pub fn install(dmg: &Path, app: &Path, mount: &Path, version: &semver::Version) -> anyhow::Result<()> {
    use std::process::Command;

    let parent = app.parent().context("flux.app has no folder")?;
    let staged = parent.join(STAGED);
    if staged.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(&staged).with_context(|| format!("clearing {}", staged.display()))?;
    }
    std::fs::create_dir_all(mount)?;
    run(Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"])
        .arg(mount)
        .arg(dmg))
    .context("mounting the disk image")?;
    let copied = (|| {
        let new = mount.join("flux.app");
        anyhow::ensure!(new.is_dir(), "the disk image has no flux.app");
        // ditto keeps what a bundle needs: signatures, extended attributes,
        // links inside it.
        run(Command::new("/usr/bin/ditto").arg(&new).arg(&staged)).context("copying the new flux.app")
    })();
    if run(Command::new("/usr/bin/hdiutil").arg("detach").arg(mount)).is_err() {
        let _ = run(Command::new("/usr/bin/hdiutil")
            .args(["detach", "-force"])
            .arg(mount));
    }
    let checked = copied.and_then(|()| check(app, &staged, version));
    if let Err(e) = checked {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e);
    }
    let aside = parent.join(format!(".flux-replaced-{}.app", std::process::id()));
    swap(app, &staged, &aside)
}

/// The new bundle is signed by the running app's team, when that one is
/// signed, and its flux-server starts and says `version`.
#[cfg(target_os = "macos")]
fn check(app: &Path, staged: &Path, version: &semver::Version) -> anyhow::Result<()> {
    use std::process::Command;

    let team = |p: &Path| -> Option<String> {
        let out = Command::new("/usr/bin/codesign")
            .arg("-dv")
            .arg(p)
            .output()
            .ok()?;
        team_of(&String::from_utf8_lossy(&out.stderr))
    };
    match team(app) {
        Some(ours) => {
            run(Command::new("/usr/bin/codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(staged))
            .context("the new flux.app's signature doesn't check out")?;
            let theirs = team(staged);
            anyhow::ensure!(
                theirs.as_deref() == Some(ours.as_str()),
                "the new flux.app is signed by {}, not by this one's team ({ours})",
                theirs.as_deref().unwrap_or("no team")
            );
        }
        None => tracing::info!("this flux.app isn't signed by a team, so neither need the new one be"),
    }
    let exe = staged.join(super::install::Layout::App.exe());
    anyhow::ensure!(
        exe.is_file(),
        "the new flux.app has no {}",
        super::install::Layout::App.exe()
    );
    super::package::try_run(&exe, version)
}

#[cfg(target_os = "macos")]
fn run(cmd: &mut std::process::Command) -> anyhow::Result<()> {
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !out.status.success() {
        anyhow::bail!(
            "{:?} failed ({}): {}",
            cmd.get_program(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn install(_dmg: &Path, _app: &Path, _mount: &Path, version: &semver::Version) -> anyhow::Result<()> {
    anyhow::bail!("flux {version}'s disk image installs on macOS only")
}

/// Where the disk image is mounted while flux copies the new app out.
pub fn mount_point(downloads: &Path) -> PathBuf {
    downloads.join("mount")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_team_comes_from_codesign() {
        let signed = "Executable=/Applications/flux.app/Contents/MacOS/flux\n\
                      Identifier=dev.vibecook.flux\nFormat=app bundle with Mach-O universal (x86_64 arm64)\n\
                      TeamIdentifier=ABCDE12345\nSealed Resources version=2 rules=13 files=40\n";
        assert_eq!(team_of(signed).as_deref(), Some("ABCDE12345"));
        assert_eq!(team_of("Signature=adhoc\nTeamIdentifier=not set\n"), None);
        assert_eq!(team_of("code object is not signed at all\n"), None);
    }

    fn bundle(at: &Path, version: &str) {
        std::fs::create_dir_all(at.join("Contents")).unwrap();
        std::fs::write(at.join("Contents/version"), version).unwrap();
    }

    fn version_at(at: &Path) -> String {
        std::fs::read_to_string(at.join("Contents/version")).unwrap()
    }

    #[test]
    fn swaps_the_new_bundle_in_and_deletes_the_old_one() {
        let d = tempfile::tempdir().unwrap();
        let (app, staged, aside) = (
            d.path().join("flux.app"),
            d.path().join(STAGED),
            d.path().join(".flux-replaced-1.app"),
        );
        bundle(&app, "old");
        bundle(&staged, "new");
        // Left over from an earlier, interrupted update.
        bundle(&aside, "older");
        swap(&app, &staged, &aside).unwrap();
        assert_eq!(version_at(&app), "new");
        assert!(!staged.exists() && !aside.exists());
    }

    #[test]
    fn a_failed_swap_puts_the_old_bundle_back() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("flux.app");
        bundle(&app, "old");
        let missing = d.path().join(STAGED);
        let err = swap(&app, &missing, &d.path().join(".aside.app")).unwrap_err();
        assert!(format!("{err:#}").contains("putting the new flux.app"), "{err:#}");
        assert_eq!(version_at(&app), "old");
    }

    #[test]
    fn says_why_it_cant_replace_itself() {
        let translocated = Path::new("/private/var/folders/x/AppTranslocation/1234/d/flux.app");
        assert!(why_not_replace(translocated).is_some_and(|w| w.contains("Applications")));
        assert!(why_not_replace(Path::new("/Volumes/flux 0.1.0/flux.app")).is_some());
        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("flux.app");
        bundle(&app, "old");
        assert_eq!(why_not_replace(&app), None);
    }
}
