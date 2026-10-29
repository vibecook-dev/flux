//! flux's own updates (docs/INSTALL.md § Updating).
//!
//! - **Checking.** Once a day, and when asked, flux reads its releases on
//!   GitHub ([`release`]) and keeps the newest one it may move to: a
//!   pre-release follows pre-releases, a stable version stable releases. On
//!   by default; `--update-check false`, the config file or the Libraries
//!   screen turns it off. GitHub sees an address and flux's version, nothing
//!   about the library.
//! - **Updating.** A release package unpacked by hand ([`install`]: an
//!   archive install) updates in place ([`package`]): download, check
//!   against `SHA256SUMS`, unpack beside the install, try the new binary,
//!   swap it in entry by entry (keeping the old one in `.flux-previous`),
//!   then restart as the new version (`main.rs`). The Windows installer's
//!   flux hands the next installer to Windows Installer ([`msi`]). The
//!   macOS app replaces its bundle with the one in the next disk image
//!   ([`app`]). Docker, Homebrew and the Linux packages update their own
//!   way, which flux names.
//! - `flux-server update` does the same from a terminal ([`cli`]), for an
//!   install the server can't write to (`/opt/flux` owned by root).

pub mod app;
pub mod cli;
pub mod install;
pub mod msi;
pub mod package;
pub mod release;
pub mod unzip;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use flux_core::api::{Event, InstallInfo, InstallKind, ReleaseInfo, UpdateInfo, UpdatePhase, UpdateProgress};
use semver::Version;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::events::EventBus;
use install::Install;
use release::Release;

/// The target this binary was built for (build.rs), naming its package.
pub const TARGET: &str = env!("FLUX_TARGET");
/// The targets releases have packages for (docs/RELEASING.md).
pub const PACKAGED_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
];
/// How often flux checks.
pub const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// The first check after a start that has never checked (or is overdue).
const FIRST_AFTER: Duration = Duration::from_secs(60);
/// Release notes lines shown with an update.
const HIGHLIGHTS: usize = 6;
/// Progress events while downloading, at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// In the data dir: the check's setting as last switched in the app
/// (`on`/`off`), and what the last check found.
pub const SETTING_FILE: &str = "update-check";
pub const STATE_FILE: &str = "update.json";
/// In the data dir: packages being downloaded.
pub const DOWNLOADS_DIR: &str = "updates";

/// A restart flux asks of itself: `main.rs` stops the server, then starts
/// `exe` with the same arguments (in place of this process on Unix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restart {
    pub exe: PathBuf,
    /// Why, for the log.
    pub why: &'static str,
}

/// What the last check found, kept across restarts.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    checked_at: Option<i64>,
    available: Option<ReleaseInfo>,
}

#[derive(Debug)]
struct Status {
    check: bool,
    checked_at: Option<i64>,
    available: Option<ReleaseInfo>,
    phase: UpdatePhase,
    progress: Option<UpdateProgress>,
    error: Option<String>,
    progress_sent: Option<Instant>,
}

/// Where the updater looks and what it is updating.
#[derive(Debug, Clone)]
pub struct Settings {
    pub install: Install,
    pub current: Version,
    pub target: String,
    /// The releases list (GitHub's API; a stand-in in tests).
    pub source: String,
    pub data_dir: PathBuf,
    /// The check's setting from the command line, environment or config
    /// file, which the app can't change; `None`: the app's, default on.
    pub fixed: Option<bool>,
}

impl Settings {
    /// This binary's. `FLUX_UPDATE_CURRENT` overrides the version the
    /// updater takes this flux for (tests of the updater, which update a
    /// binary to itself).
    pub fn for_server(data_dir: &Path, fixed: Option<bool>, source: Option<String>) -> Settings {
        let pretend = std::env::var("FLUX_UPDATE_CURRENT")
            .ok()
            .and_then(|v| Version::parse(v.trim()).ok());
        Settings {
            install: Install::detect(),
            current: pretend.unwrap_or_else(|| {
                Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
            }),
            target: TARGET.to_string(),
            source: source.unwrap_or_else(|| release::RELEASES_URL.to_string()),
            data_dir: data_dir.to_path_buf(),
            fixed,
        }
    }
}

pub struct Updater {
    s: Settings,
    client: reqwest::Client,
    events: Option<EventBus>,
    status: Mutex<Status>,
    /// One update at a time.
    busy: Arc<tokio::sync::Mutex<()>>,
    /// Wakes the daily loop (the check was switched on).
    wake: Notify,
    restart: Mutex<Option<Restart>>,
    restart_asked: Notify,
    /// The running binary, for a restart without an update.
    exe: Option<PathBuf>,
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The client every updater request goes through.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!(
            "flux/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/vibecook-dev/flux)"
        ))
        .connect_timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

impl Updater {
    pub fn new(s: Settings, events: Option<EventBus>) -> Updater {
        let saved: Saved = std::fs::read(s.data_dir.join(STATE_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let check = s
            .fixed
            .unwrap_or_else(|| persisted_setting(&s.data_dir).unwrap_or(true));
        Updater {
            status: Mutex::new(Status {
                check,
                checked_at: saved.checked_at,
                available: saved.available,
                phase: UpdatePhase::Idle,
                progress: None,
                error: None,
                progress_sent: None,
            }),
            client: http_client(),
            events,
            busy: Arc::default(),
            wake: Notify::new(),
            restart: Mutex::new(None),
            restart_asked: Notify::new(),
            exe: std::env::current_exe()
                .ok()
                .map(|e| e.canonicalize().unwrap_or(e)),
            s,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.s
    }

    fn status(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Everything a person needs to see (`GET /api/v1/admin/update`).
    pub fn info(&self) -> UpdateInfo {
        let st = self.status();
        UpdateInfo {
            current: self.s.current.to_string(),
            check: st.check,
            check_fixed: self.s.fixed.is_some(),
            checked_at: st.checked_at,
            // What the last check found, unless this is it now (after the update).
            available: st
                .available
                .clone()
                .filter(|a| Version::parse(&a.version).is_ok_and(|v| v > self.s.current)),
            install: self.install_info(st.available.as_ref().map(|a| a.version.as_str())),
            phase: st.phase,
            progress: st.progress,
            error: st.error.clone(),
            previous: self.previous_version(),
        }
    }

    /// The version kept by the last update, if any.
    pub fn previous_version(&self) -> Option<String> {
        let dir = self.s.install.previous_dir()?;
        if !self.s.install.exe_in(&dir).is_file() {
            return None;
        }
        std::fs::read_to_string(dir.join(package::PREVIOUS_VERSION))
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// How flux was installed here.
    pub fn install_kind(&self) -> InstallKind {
        self.s.install.kind
    }

    /// How this install updates.
    pub fn install_info(&self, to: Option<&str>) -> InstallInfo {
        let to = to.unwrap_or("<version>");
        let none = |kind, why_not: Option<String>, command: Option<String>| InstallInfo {
            kind,
            can_apply: false,
            why_not,
            command,
        };
        match self.s.install.kind {
            InstallKind::Archive => {
                let root = self.s.install.root.as_deref();
                let sudo = if cfg!(windows) {
                    "flux-server update"
                } else {
                    "sudo flux-server update"
                };
                if !PACKAGED_TARGETS.contains(&self.s.target.as_str()) {
                    none(
                        InstallKind::Archive,
                        Some(format!("Releases have no package for this system ({}).", self.s.target)),
                        None,
                    )
                } else if !root.is_some_and(install::writable) {
                    none(
                        InstallKind::Archive,
                        Some(if cfg!(windows) {
                            "flux can't write to its own folder, so it can't update itself from here. Run the command in a terminal opened as administrator.".to_string()
                        } else {
                            "flux can't write to its own folder, so it can't update itself from here. Run the command on the server.".to_string()
                        }),
                        Some(sudo.to_string()),
                    )
                } else {
                    InstallInfo {
                        kind: InstallKind::Archive,
                        can_apply: true,
                        why_not: None,
                        command: Some("flux-server update".to_string()),
                    }
                }
            }
            InstallKind::Docker => none(
                InstallKind::Docker,
                Some(format!(
                    "The container updates from the new image, ghcr.io/vibecook-dev/flux:{to}. With Compose, pull it and recreate the container."
                )),
                Some("docker compose pull && docker compose up -d".to_string()),
            ),
            InstallKind::Homebrew => none(
                InstallKind::Homebrew,
                None,
                Some("brew upgrade vibecook-dev/tap/flux".to_string()),
            ),
            InstallKind::Deb => {
                let deb = format!("flux-{to}-{}.deb", if cfg!(target_arch = "aarch64") { "arm64" } else { "amd64" });
                none(
                    InstallKind::Deb,
                    Some("Install the new .deb with apt; the service restarts as the new version.".to_string()),
                    Some(format!(
                        "curl -fLO https://github.com/vibecook-dev/flux/releases/download/v{to}/{deb} && sudo apt install ./{deb}"
                    )),
                )
            }
            InstallKind::Rpm => none(
                InstallKind::Rpm,
                Some("Install the new .rpm with dnf; the service restarts as the new version.".to_string()),
                Some(format!(
                    "sudo dnf install https://github.com/vibecook-dev/flux/releases/download/v{to}/flux-{to}-{}.rpm",
                    std::env::consts::ARCH
                )),
            ),
            InstallKind::Msi if is_admin() => InstallInfo {
                kind: InstallKind::Msi,
                can_apply: true,
                why_not: None,
                command: Some("flux-server update".to_string()),
            },
            InstallKind::Msi => none(
                InstallKind::Msi,
                Some(
                    "Windows Installer needs an administrator: run the command in a terminal opened as \
                     administrator, or the new installer from the release page."
                        .to_string(),
                ),
                Some("flux-server update".to_string()),
            ),
            InstallKind::App => {
                let why_not = if !PACKAGED_TARGETS.contains(&self.s.target.as_str()) {
                    Some(format!("Releases have no app for this system ({}).", self.s.target))
                } else {
                    match self.s.install.root.as_deref() {
                        Some(app) => app::why_not_replace(app),
                        None => Some("flux can't find its flux.app.".to_string()),
                    }
                };
                InstallInfo {
                    kind: InstallKind::App,
                    can_apply: why_not.is_none(),
                    why_not,
                    command: None,
                }
            }
            InstallKind::Source => none(
                InstallKind::Source,
                Some("This flux was built from source: update the checkout and build it again.".to_string()),
                None,
            ),
        }
    }

    fn broadcast(&self) {
        if let Some(bus) = &self.events {
            bus.send(Event::Update { info: self.info() });
        }
    }

    fn set_phase(&self, phase: UpdatePhase) {
        {
            let mut st = self.status();
            st.phase = phase;
            if phase != UpdatePhase::Downloading {
                st.progress = None;
            }
        }
        self.broadcast();
    }

    fn progress(&self, done: u64, total: u64) {
        let send = {
            let mut st = self.status();
            st.progress = Some(UpdateProgress { done, total });
            let due = st.progress_sent.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) || done == total;
            if due {
                st.progress_sent = Some(Instant::now());
            }
            due
        };
        if send {
            self.broadcast();
        }
    }

    fn save(&self) {
        let saved = {
            let st = self.status();
            Saved {
                checked_at: st.checked_at,
                available: st.available.clone(),
            }
        };
        let path = self.s.data_dir.join(STATE_FILE);
        let tmp = path.with_extension("json.part");
        let ok = serde_json::to_vec(&saved)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = ok {
            tracing::debug!(error = %e, "could not save {}", path.display());
        }
    }

    /// Switches the daily check on or off (the app's setting, kept in the
    /// data dir). Refused when the server's configuration sets it.
    pub fn set_check(&self, on: bool) -> Result<UpdateInfo, String> {
        if self.s.fixed.is_some() {
            return Err(
                "The server's configuration sets this (--update-check, FLUX_UPDATE_CHECK or update_check)."
                    .into(),
            );
        }
        if let Err(e) = std::fs::write(self.s.data_dir.join(SETTING_FILE), if on { "on" } else { "off" }) {
            return Err(format!("Couldn't save the setting: {e}"));
        }
        self.status().check = on;
        if on {
            self.wake.notify_one();
        }
        self.broadcast();
        Ok(self.info())
    }

    /// The releases, newest first.
    pub async fn releases(&self) -> anyhow::Result<Vec<Release>> {
        let res = self
            .client
            .get(&self.s.source)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("GitHub didn't answer")?;
        let status = res.status();
        if !status.is_success() {
            anyhow::bail!("GitHub answered {status}");
        }
        release::parse(&res.bytes().await?).context("GitHub's answer wasn't a list of releases")
    }

    /// The newest release this server may update to.
    pub fn pick<'a>(&self, releases: &'a [Release]) -> Option<&'a Release> {
        release::newest_for(releases, &self.s.current)
    }

    /// Asks GitHub now; the result is in [`info`](Self::info). Not while an
    /// update runs.
    pub async fn check(&self) -> UpdateInfo {
        {
            let mut st = self.status();
            if matches!(
                st.phase,
                UpdatePhase::Downloading
                    | UpdatePhase::Verifying
                    | UpdatePhase::Installing
                    | UpdatePhase::Restarting
            ) {
                drop(st);
                return self.info();
            }
            st.phase = UpdatePhase::Checking;
        }
        self.broadcast();
        let result = self.releases().await;
        {
            let mut st = self.status();
            st.phase = UpdatePhase::Idle;
            match &result {
                Ok(list) => {
                    st.checked_at = Some(now_ms());
                    st.available = self.pick(list).map(release_info);
                    st.error = None;
                }
                Err(e) => st.error = Some(format!("The check for updates failed: {e:#}")),
            }
        }
        match &result {
            Ok(_) => {
                if let Some(a) = &self.status().available {
                    tracing::info!(current = %self.s.current, available = %a.version, "a newer flux is out");
                }
                self.save();
            }
            Err(e) => tracing::info!(error = format!("{e:#}"), "update check failed"),
        }
        self.broadcast();
        self.info()
    }

    /// The daily check, until shutdown.
    pub fn spawn(self: &Arc<Self>, shutdown: CancellationToken) -> JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let wait = me.next_check_in();
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(wait) => {}
                    _ = me.wake.notified() => {}
                }
                if me.status().check {
                    me.check().await;
                }
            }
        })
    }

    /// Until the next check is due: a day after the last one, at least a
    /// minute after startup.
    fn next_check_in(&self) -> Duration {
        let st = self.status();
        if !st.check {
            return EVERY;
        }
        match st.checked_at {
            None => FIRST_AFTER,
            Some(at) => {
                let due = at + EVERY.as_millis() as i64 - now_ms();
                Duration::from_millis(due.max(0) as u64).clamp(FIRST_AFTER, EVERY)
            }
        }
    }

    /// Starts updating to the newest release (an archive install that can
    /// write to its folder); the work goes on in the background, with
    /// [`Event::Update`]s, and ends in a restart.
    pub fn apply(self: &Arc<Self>) -> Result<UpdateInfo, String> {
        let install = self.install_info(None);
        if !install.can_apply {
            return Err(install
                .why_not
                .unwrap_or_else(|| "This install doesn't update itself.".into()));
        }
        let guard = self
            .busy
            .clone()
            .try_lock_owned()
            .map_err(|_| "An update is already running.".to_string())?;
        {
            let mut st = self.status();
            st.error = None;
            st.progress = None;
        }
        self.set_phase(UpdatePhase::Downloading);
        let me = self.clone();
        tokio::spawn(async move {
            let _one_at_a_time = guard;
            match me.update().await {
                Ok(Some(restart)) => {
                    me.set_phase(UpdatePhase::Restarting);
                    me.ask_restart(restart);
                }
                // Windows Installer stops flux and starts the new version.
                Ok(None) => me.set_phase(UpdatePhase::Restarting),
                Err(e) => {
                    let why = format!("{e:#}");
                    tracing::warn!(error = %why, "update failed; flux carries on as it was");
                    me.status().error = Some(why);
                    me.set_phase(UpdatePhase::Failed);
                }
            }
        });
        Ok(self.info())
    }

    /// Download, check, unpack, try, swap in: the restart that follows. For
    /// the Windows installer's flux, download, check and hand the installer
    /// over (`None`: Windows Installer restarts flux).
    async fn update(&self) -> anyhow::Result<Option<Restart>> {
        let releases = self.releases().await?;
        let release = self.pick(&releases).cloned().context("flux is up to date")?;
        let dir = self.s.data_dir.join(DOWNLOADS_DIR);
        if self.s.install.kind == InstallKind::Msi {
            let msi = self
                .download_installer(&release, &dir, |done, total| self.progress(done, total))
                .await?;
            self.set_phase(UpdatePhase::Installing);
            self.hand_over(&msi, &release.version, None)?;
            tracing::info!(from = %self.s.current, to = %release.version, "Windows Installer is installing the new version");
            return Ok(None);
        }
        if self.s.install.kind == InstallKind::App {
            let dmg = self
                .download_app(&release, &dir, |done, total| self.progress(done, total))
                .await?;
            self.set_phase(UpdatePhase::Verifying);
            self.install_app(&dmg, &dir, &release.version).await?;
            let _ = std::fs::remove_dir_all(&dir);
            tracing::info!(from = %self.s.current, to = %release.version, "updated flux.app; restarting as the new version");
            return Ok(Some(Restart {
                exe: self.s.install.exe().context("no flux.app to restart")?,
                why: "update",
            }));
        }
        let top = self
            .prepare(
                &release,
                &dir,
                |done, total| self.progress(done, total),
                || self.set_phase(UpdatePhase::Verifying),
            )
            .await?;
        self.set_phase(UpdatePhase::Installing);
        let current = self.s.current.to_string();
        let install = self.s.install.clone();
        tokio::task::spawn_blocking(move || put_in_place(&install, &top, &current)).await??;
        let _ = std::fs::remove_dir_all(&dir);
        tracing::info!(from = %self.s.current, to = %release.version, "updated; restarting as the new version");
        Ok(Some(Restart {
            exe: self.s.install.exe().context("no package to restart")?,
            why: "update",
        }))
    }

    /// `release`'s `SHA256SUMS`.
    async fn sums(&self, release: &Release) -> anyhow::Result<String> {
        let asset = release
            .asset(release::SUMS)
            .context("the release has no SHA256SUMS")?;
        Ok(self
            .client
            .get(&asset.url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .context("downloading SHA256SUMS")?
            .text()
            .await?)
    }

    /// Downloads `release`'s Windows installer into `dir`, a folder for
    /// SYSTEM and Administrators only (emptied first), and checks it against
    /// `SHA256SUMS`: returns its path.
    pub async fn download_installer(
        &self,
        release: &Release,
        dir: &Path,
        progress: impl FnMut(u64, u64),
    ) -> anyhow::Result<PathBuf> {
        let name = release::installer_name(&release.version, &self.s.target);
        self.download_asset(release, &name, "Windows installer", dir, progress)
            .await
    }

    /// Downloads `release`'s disk image of the macOS app into `dir` (emptied
    /// first) and checks it against `SHA256SUMS`: returns its path.
    pub async fn download_app(
        &self,
        release: &Release,
        dir: &Path,
        progress: impl FnMut(u64, u64),
    ) -> anyhow::Result<PathBuf> {
        let name = release::app_name(&release.version);
        self.download_asset(release, &name, "disk image of the macOS app", dir, progress)
            .await
    }

    /// Puts the flux.app in disk image `dmg` in place of this one ([`app`]);
    /// `dir` holds the download.
    pub async fn install_app(&self, dmg: &Path, dir: &Path, version: &Version) -> anyhow::Result<()> {
        let app = self
            .s
            .install
            .root
            .clone()
            .context("flux can't find its flux.app")?;
        let (dmg, mount, version) = (dmg.to_path_buf(), app::mount_point(dir), version.clone());
        tokio::task::spawn_blocking(move || app::install(&dmg, &app, &mount, &version)).await?
    }

    /// Downloads `release`'s asset `name` (a `what`) into `dir`, a private
    /// folder emptied first, and checks it against `SHA256SUMS`.
    async fn download_asset(
        &self,
        release: &Release,
        name: &str,
        what: &str,
        dir: &Path,
        mut progress: impl FnMut(u64, u64),
    ) -> anyhow::Result<PathBuf> {
        let asset = release
            .asset(name)
            .with_context(|| format!("release {} has no {what} ({name})", release.version))?
            .clone();
        let sums = self.sums(release).await?;
        let sha = release::sum_of(&sums, name).with_context(|| format!("SHA256SUMS lists no {name}"))?;
        private_dir(dir)?;
        let file = dir.join(name);
        let total = asset.size;
        package::download(&self.client, &asset.url, &file, total, &sha, |done| {
            progress(done, total)
        })
        .await?;
        Ok(file)
    }

    /// Hands installer `msi` of `version` to a copy of flux that runs
    /// Windows Installer on it ([`msi`]), once process `after` (if any) ends.
    pub fn hand_over(&self, msi: &Path, version: &Version, after: Option<u32>) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            msi::hand_over(msi, &version.to_string(), &self.s.data_dir, after)
        }
        #[cfg(not(windows))]
        {
            let _ = (msi, after);
            anyhow::bail!("flux {version}'s Windows installer runs on Windows only")
        }
    }

    /// The server, as it starts: what Windows Installer did with the last
    /// update this flux handed over, waiting for it while it may still run
    /// (this flux is then the old version, back after a failed install).
    /// The update panel says why it failed.
    pub fn follow_installer(self: &Arc<Self>, shutdown: CancellationToken) {
        if self.s.install.kind != InstallKind::Msi {
            return;
        }
        let dir = self.s.data_dir.join(DOWNLOADS_DIR);
        if self.take_installer_outcome(&dir) {
            return;
        }
        let Some(pending) = msi::pending(&dir) else {
            return;
        };
        if now_ms() - pending.started_at > INSTALLER_WAIT.as_millis() as i64 {
            let _ = std::fs::remove_file(dir.join(msi::PENDING));
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + INSTALLER_WAIT;
            while Instant::now() < deadline {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
                if me.take_installer_outcome(&dir) {
                    return;
                }
            }
        });
    }

    /// Takes Windows Installer's outcome from `dir`, if it is in (true), and
    /// says why the update failed, if it did.
    fn take_installer_outcome(&self, dir: &Path) -> bool {
        let Some(outcome) = msi::take_outcome(dir) else {
            return false;
        };
        msi::tidy(dir);
        match msi::failure(&outcome, &self.s.current.to_string()) {
            Some(why) => {
                tracing::warn!(error = %why, "the update failed; flux carries on as it was");
                self.status().error = Some(why);
                self.set_phase(UpdatePhase::Failed);
            }
            None => {
                tracing::info!(version = %outcome.version, code = outcome.code, "Windows Installer installed flux")
            }
        }
        true
    }

    /// Downloads `release`'s package for this target into `dir`, checks it,
    /// unpacks it beside the install and tries it: returns its folder.
    /// `progress` hears the download; `verifying` when it is done.
    pub async fn prepare(
        &self,
        release: &Release,
        dir: &Path,
        progress: impl FnMut(u64, u64),
        verifying: impl FnOnce(),
    ) -> anyhow::Result<PathBuf> {
        let name = release::package_name(&release.version, &self.s.target);
        let pkg = release
            .asset(&name)
            .with_context(|| {
                format!(
                    "release {} has no package for this system ({})",
                    release.version, self.s.target
                )
            })?
            .clone();
        let sums = self.sums(release).await?;
        let sha = release::sum_of(&sums, &name).with_context(|| format!("SHA256SUMS lists no {name}"))?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let file = dir.join(&name);
        let total = pkg.size;
        let mut progress = progress;
        package::download(&self.client, &pkg.url, &file, total, &sha, |done| {
            progress(done, total)
        })
        .await?;
        verifying();
        let staging = self.s.install.staging_dir().context("no package folder")?;
        let install = self.s.install.clone();
        let version = release.version.clone();
        let top = tokio::task::spawn_blocking(move || -> anyhow::Result<PathBuf> {
            let top = package::unpack(&file, &staging)?;
            let exe = install.exe_in(&top);
            anyhow::ensure!(exe.is_file(), "the package has no {}", install.layout.exe());
            package::try_run(&exe, &version)?;
            Ok(top)
        })
        .await??;
        Ok(top)
    }

    /// Asks `main.rs` to restart (after an update, or for `flux-server update`
    /// run from a terminal).
    pub fn ask_restart(&self, restart: Restart) {
        *self.restart.lock().unwrap_or_else(|p| p.into_inner()) = Some(restart);
        self.restart_asked.notify_one();
    }

    /// A restart of the running binary as it is now on disk (the package's
    /// path, so a swapped-in version starts).
    pub fn restart_in_place(&self, why: &'static str) -> Option<Restart> {
        let exe = self.s.install.exe().or_else(|| self.exe.clone())?;
        Some(Restart { exe, why })
    }

    /// Resolves when a restart is asked for.
    pub async fn restart_asked(&self) -> Restart {
        loop {
            if let Some(r) = self.restart.lock().unwrap_or_else(|p| p.into_inner()).take() {
                return r;
            }
            self.restart_asked.notified().await;
        }
    }
}

/// How long flux waits for Windows Installer to finish an update it
/// handed over.
const INSTALLER_WAIT: Duration = Duration::from_secs(30 * 60);

/// Whether this flux can run Windows Installer (an administrator).
fn is_admin() -> bool {
    #[cfg(windows)]
    {
        crate::win32::is_admin()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Makes `dir` empty, and on Windows open to SYSTEM and Administrators
/// only: what is downloaded there runs as an administrator.
fn private_dir(dir: &Path) -> anyhow::Result<()> {
    if dir.exists() {
        let _ = std::fs::remove_dir_all(dir);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(windows)]
    crate::win32::restrict_to_admins(dir).with_context(|| format!("restricting {}", dir.display()))?;
    // Whatever another user could add before that goes too.
    for entry in std::fs::read_dir(dir)?.flatten() {
        let p = entry.path();
        let gone = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        gone.with_context(|| format!("clearing {}", p.display()))?;
    }
    Ok(())
}

/// The check's setting as last switched in the app.
pub fn persisted_setting(data_dir: &Path) -> Option<bool> {
    match std::fs::read_to_string(data_dir.join(SETTING_FILE)).ok()?.trim() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// A release as the app shows it.
pub fn release_info(r: &Release) -> ReleaseInfo {
    ReleaseInfo {
        version: r.version.to_string(),
        published_at: r.published_at,
        url: r.url.clone(),
        highlights: release::highlights(&r.notes, HIGHLIGHTS),
    }
}

/// Swaps the unpacked package at `top` into the install, noting the version
/// it replaces, and clears the staging folder.
pub fn put_in_place(install: &Install, top: &Path, replaced: &str) -> anyhow::Result<()> {
    let root = install.root.as_deref().context("no package folder")?;
    let previous = install.previous_dir().context("no package folder")?;
    package::swap_in(root, top, &previous)?;
    std::fs::write(previous.join(package::PREVIOUS_VERSION), format!("{replaced}\n"))?;
    if let Some(staging) = install.staging_dir() {
        let _ = std::fs::remove_dir_all(staging);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn updater(kind: InstallKind) -> Updater {
        let d = std::env::temp_dir();
        let mut s = Settings::for_server(&d, Some(false), None);
        s.install = Install {
            kind,
            root: None,
            layout: install::Layout::native(),
        };
        Updater::new(s, None)
    }

    /// The .deb, .rpm and Homebrew are told the command for the new version,
    /// with the asset named as the release has it (release-assets.sh).
    #[test]
    fn packages_are_told_what_to_run() {
        let deb = updater(InstallKind::Deb).install_info(Some("1.2.0-beta.1"));
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "amd64"
        };
        assert_eq!(
            deb.command.as_deref(),
            Some(
                format!(
                    "curl -fLO https://github.com/vibecook-dev/flux/releases/download/v1.2.0-beta.1/flux-1.2.0-beta.1-{arch}.deb && sudo apt install ./flux-1.2.0-beta.1-{arch}.deb"
                )
                .as_str()
            )
        );
        assert!(!deb.can_apply);
        let rpm = updater(InstallKind::Rpm).install_info(Some("1.2.0"));
        assert!(
            rpm.command
                .as_deref()
                .is_some_and(|c| c.ends_with(&format!("/v1.2.0/flux-1.2.0-{}.rpm", std::env::consts::ARCH))),
            "{rpm:?}"
        );
        let brew = updater(InstallKind::Homebrew).install_info(None);
        assert_eq!(
            brew.command.as_deref(),
            Some("brew upgrade vibecook-dev/tap/flux")
        );
    }

    /// The Windows installer's flux updates itself as an administrator (the
    /// service is one); otherwise it says what to run, and where.
    #[test]
    fn the_installer_updates_as_an_administrator() {
        let msi = updater(InstallKind::Msi).install_info(Some("1.2.0"));
        assert_eq!(msi.command.as_deref(), Some("flux-server update"));
        assert_eq!(msi.can_apply, is_admin());
        if !msi.can_apply {
            assert!(
                msi.why_not
                    .as_deref()
                    .is_some_and(|w| w.contains("administrator")),
                "{msi:?}"
            );
        }
    }
}
