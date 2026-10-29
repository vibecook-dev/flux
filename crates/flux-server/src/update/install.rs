//! How this flux was installed, and where: the marker each package carries,
//! and the folder it was unpacked to.
//!
//! The release packages carry `install-kind` (`archive`) beside the web
//! client: `share/flux/install-kind` in the Unix tree, `install-kind` in the
//! Windows folder. Packages made from them write their own word (Homebrew,
//! the Debian and RPM packages, the Windows installer, the macOS app, which
//! has it in `flux.app/Contents/Resources`), and the Docker image sets
//! [`KIND_ENV`]. No marker: a build from source.

use std::path::{Path, PathBuf};

use flux_core::api::InstallKind;

/// The marker's file name.
pub const MARKER: &str = "install-kind";
/// The install kind from the environment, which wins over the marker (the
/// Docker image is made from the Linux package, whose marker says `archive`).
pub const KIND_ENV: &str = "FLUX_INSTALL_KIND";
/// Inside the package folder: the version an update replaced (for
/// `flux-server update --rollback`), and the new one while it is unpacked.
pub const PREVIOUS_DIR: &str = ".flux-previous";
pub const STAGING_DIR: &str = ".flux-update";

/// The top-level entries of a flux package, in either layout. An update
/// replaces these (whether or not the new package has each one); anything
/// else in the folder was put there by someone and stays.
pub const PACKAGE_ENTRIES: &[&str] = &[
    "bin",
    "share",
    "web",
    MARKER,
    "flux-server.exe",
    "flux.exe",
    "sidecar-slim.exe",
    "LICENSE",
    "LICENSE.txt",
    "README.md",
    "README.txt",
    "THIRD_PARTY_LICENSES",
    "THIRD_PARTY_LICENSES.txt",
];

/// How a package lays out its files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `bin/flux-server`, `share/flux/web/` (macOS, Linux).
    Unix,
    /// `flux-server.exe`, `web\` in one folder (Windows).
    Flat,
    /// The macOS app: `flux.app/Contents/Helpers/flux-server`,
    /// `Contents/Resources/web/`. It updates as a whole bundle
    /// ([`super::app`]), never entry by entry: anything changed inside a
    /// signed bundle breaks its signature.
    App,
}

impl Layout {
    /// This platform's.
    pub fn native() -> Layout {
        if cfg!(windows) {
            Layout::Flat
        } else {
            Layout::Unix
        }
    }

    /// The flux-server binary of a package in this layout, from its top.
    pub fn exe(self) -> &'static str {
        match self {
            Layout::Unix => "bin/flux-server",
            Layout::Flat => "flux-server.exe",
            Layout::App => "Contents/Helpers/flux-server",
        }
    }
}

/// The marker inside `flux.app`.
pub const APP_MARKER: &str = "Contents/Resources/install-kind";

/// This flux's install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub kind: InstallKind,
    /// The package's folder (`/opt/flux`, `%LOCALAPPDATA%\Programs\flux`):
    /// `None` without a package (a build from source).
    pub root: Option<PathBuf>,
    pub layout: Layout,
}

impl Install {
    /// The running binary's install.
    pub fn detect() -> Install {
        let env = std::env::var(KIND_ENV).ok();
        match std::env::current_exe() {
            Ok(exe) => Install::at(&exe.canonicalize().unwrap_or(exe), env.as_deref()),
            Err(_) => Install {
                kind: env.as_deref().and_then(parse_kind).unwrap_or(InstallKind::Source),
                root: None,
                layout: Layout::native(),
            },
        }
    }

    /// The install of the flux-server binary at `exe` (its real path, links
    /// resolved); `env_kind` is [`KIND_ENV`]'s value.
    pub fn at(exe: &Path, env_kind: Option<&str>) -> Install {
        let dir = exe.parent();
        // flux.app/Contents/Helpers/flux-server
        let app = dir
            .filter(|d| d.file_name().is_some_and(|n| n == "Helpers"))
            .and_then(Path::parent)
            .filter(|c| c.file_name().is_some_and(|n| n == "Contents"))
            .and_then(Path::parent)
            .filter(|top| top.join(APP_MARKER).is_file());
        let unix = dir
            .and_then(Path::parent)
            .filter(|top| top.join("share").join("flux").join(MARKER).is_file());
        let flat = dir.filter(|d| d.join(MARKER).is_file());
        let (root, layout, marker) = match (unix, flat) {
            _ if app.is_some() => (app, Layout::App, app.map(|top| top.join(APP_MARKER))),
            (Some(top), _) => (
                Some(top),
                Layout::Unix,
                Some(top.join("share").join("flux").join(MARKER)),
            ),
            (None, Some(d)) => (Some(d), Layout::Flat, Some(d.join(MARKER))),
            (None, None) => (None, Layout::native(), None),
        };
        let marked = marker
            .and_then(|m| std::fs::read_to_string(m).ok())
            .and_then(|s| parse_kind(&s));
        Install {
            kind: env_kind
                .and_then(parse_kind)
                .or(marked)
                .unwrap_or(InstallKind::Source),
            root: root.map(Path::to_path_buf),
            layout,
        }
    }

    /// The flux-server binary of the package at `top` (this install's, or a
    /// new one being put in place).
    pub fn exe_in(&self, top: &Path) -> PathBuf {
        top.join(self.layout.exe())
    }

    /// This install's flux-server binary, when it is a package.
    pub fn exe(&self) -> Option<PathBuf> {
        self.root.as_deref().map(|r| self.exe_in(r))
    }

    /// Where the version an update replaced is kept (none for the app,
    /// which keeps nothing in its signed bundle).
    pub fn previous_dir(&self) -> Option<PathBuf> {
        self.package_dir().map(|r| r.join(PREVIOUS_DIR))
    }

    /// Where a new version is unpacked before it goes in (the app's goes
    /// beside the bundle: [`super::app`]).
    pub fn staging_dir(&self) -> Option<PathBuf> {
        self.package_dir().map(|r| r.join(STAGING_DIR))
    }

    /// The package's folder when updates change it entry by entry.
    fn package_dir(&self) -> Option<&Path> {
        self.root.as_deref().filter(|_| self.layout != Layout::App)
    }
}

/// The install kind a marker (or [`KIND_ENV`]) names.
pub fn parse_kind(s: &str) -> Option<InstallKind> {
    Some(match s.trim() {
        "archive" => InstallKind::Archive,
        "docker" => InstallKind::Docker,
        "homebrew" => InstallKind::Homebrew,
        "deb" => InstallKind::Deb,
        "rpm" => InstallKind::Rpm,
        "msi" => InstallKind::Msi,
        "app" => InstallKind::App,
        "source" => InstallKind::Source,
        _ => return None,
    })
}

/// Whether this process can change the package folder: an update renames
/// entries in it and unpacks beside them. Tried, not inferred from modes
/// (ACLs, read-only mounts).
pub fn writable(root: &Path) -> bool {
    let probe = root.join(format!(".flux-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(layout: Layout, kind: &str) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let top = d.path().join("flux-0.1.0-x");
        let exe = top.join(layout.exe());
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        let marker = match layout {
            Layout::Unix => top.join("share/flux").join(MARKER),
            Layout::Flat => top.join(MARKER),
            Layout::App => top.join(APP_MARKER),
        };
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(marker, format!("{kind}\n")).unwrap();
        (d, exe)
    }

    #[test]
    fn a_release_package_is_an_archive_install_in_its_folder() {
        for layout in [Layout::Unix, Layout::Flat] {
            let (d, exe) = package(layout, "archive");
            let i = Install::at(&exe, None);
            let top = d.path().join("flux-0.1.0-x");
            assert_eq!(
                i,
                Install {
                    kind: InstallKind::Archive,
                    root: Some(top.clone()),
                    layout
                }
            );
            assert_eq!(i.exe(), Some(exe));
            assert_eq!(i.previous_dir(), Some(top.join(PREVIOUS_DIR)));
        }
    }

    #[test]
    fn packages_made_from_it_say_what_they_are_and_the_environment_wins() {
        let (_d, exe) = package(Layout::Unix, "homebrew");
        assert_eq!(Install::at(&exe, None).kind, InstallKind::Homebrew);
        let (_d, exe) = package(Layout::Unix, "archive");
        assert_eq!(Install::at(&exe, Some("docker")).kind, InstallKind::Docker);
        // Nonsense in either place counts for nothing.
        assert_eq!(Install::at(&exe, Some("snap")).kind, InstallKind::Archive);
        let (_d, exe) = package(Layout::Flat, "zip?");
        assert_eq!(Install::at(&exe, None).kind, InstallKind::Source);
    }

    #[test]
    fn the_macos_app_is_its_bundle() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("flux.app");
        let exe = app.join(Layout::App.exe());
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        // Without its marker, a bundle is no package.
        assert_eq!(Install::at(&exe, None).root, None);
        std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        std::fs::write(app.join(APP_MARKER), "app\n").unwrap();
        let i = Install::at(&exe, None);
        assert_eq!(
            i,
            Install {
                kind: InstallKind::App,
                root: Some(app.clone()),
                layout: Layout::App
            }
        );
        assert_eq!(i.exe(), Some(exe));
        // Nothing is kept or unpacked inside the signed bundle.
        assert_eq!(i.previous_dir(), None);
        assert_eq!(i.staging_dir(), None);
    }

    #[test]
    fn no_marker_is_a_build_from_source() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("target/release/flux-server");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        let i = Install::at(&exe, None);
        assert_eq!((i.kind, i.root.as_deref()), (InstallKind::Source, None));
        assert_eq!(i.exe(), None);
    }

    #[test]
    fn a_folder_it_can_write_to_is_found_writable() {
        let d = tempfile::tempdir().unwrap();
        assert!(writable(d.path()));
        assert!(!writable(&d.path().join("missing")));
        assert_eq!(
            std::fs::read_dir(d.path()).unwrap().count(),
            0,
            "the probe is gone"
        );
    }
}
