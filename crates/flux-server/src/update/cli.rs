//! `flux-server update`: the updater from a terminal.
//!
//! ```text
//! flux-server update              update to the newest release, then restart the running flux
//! flux-server update --check      only say whether one is out (exit 10 when it is)
//! flux-server update --rollback   put back the version the last update replaced (flux stopped)
//! ```
//!
//! For an install the server can't write to (`sudo flux-server update`), and
//! for people who'd rather type. Pass the same `--config` / `--data-dir` as
//! the server, so the restart reaches the right one.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use flux_core::api::InstallKind;
use semver::Version;

use super::{package, put_in_place, Settings, Updater};
use crate::config::Config;
use crate::server::SERVER_FILE;

#[derive(Debug, Default, clap::Args)]
pub struct UpdateArgs {
    /// Only say whether a newer version is out (exit status 10 when one is).
    #[arg(long, conflicts_with = "rollback")]
    pub check: bool,
    /// Put back the version the last update replaced (stop flux first).
    #[arg(long)]
    pub rollback: bool,
    /// Don't ask before changing anything.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// Up to date, or done.
const OK: u8 = 0;
const FAILED: u8 = 1;
/// This install doesn't update this way.
const NOT_HERE: u8 = 2;
/// `--check`: a newer version is out.
const AVAILABLE: u8 = 10;

pub fn run(cfg: &Config, args: &UpdateArgs) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("flux-server: cannot start the runtime: {e}");
            return ExitCode::from(FAILED);
        }
    };
    ExitCode::from(rt.block_on(run_async(cfg, args)))
}

async fn run_async(cfg: &Config, args: &UpdateArgs) -> u8 {
    let updater = Updater::new(
        Settings::for_server(&cfg.data_dir, Some(true), cfg.update_source.clone()),
        None,
    );
    let s = updater.settings().clone();
    if args.rollback {
        return rollback(cfg, &updater, args.yes).await;
    }
    let releases = match updater.releases().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("flux-server: the check for updates failed: {e:#}");
            return FAILED;
        }
    };
    let Some(release) = updater.pick(&releases).cloned() else {
        println!("flux {} is up to date.", s.current);
        return OK;
    };
    println!(
        "flux {} is out (this is {}): {}",
        release.version, s.current, release.url
    );
    for line in super::release::highlights(&release.notes, 6) {
        println!("  - {line}");
    }
    if args.check {
        return AVAILABLE;
    }
    let how = updater.install_info(Some(&release.version.to_string()));
    if !how.can_apply {
        if let Some(why) = &how.why_not {
            eprintln!("{why}");
        }
        if let Some(cmd) = how
            .command
            .as_deref()
            .filter(|c| !c.ends_with("flux-server update"))
        {
            eprintln!("  {cmd}");
        }
        if s.install.kind == InstallKind::Archive {
            if let Some(root) = &s.install.root {
                eprintln!("(flux-server can't write to {})", root.display());
            }
            return FAILED;
        }
        return NOT_HERE;
    }
    if !args.yes && !confirm(&format!("Install flux {}?", release.version), true) {
        println!("Nothing changed.");
        return OK;
    }
    let dir = std::env::temp_dir().join(format!("flux-update-{}", std::process::id()));
    let tty = std::io::stdout().is_terminal();
    let shown = std::cell::Cell::new(0u64);
    let progress = |done: u64, total: u64| {
        // A line per 10% when it isn't a terminal; a counter in place when it is.
        let pct = done * 100 / total.max(1);
        if tty {
            print!(
                "\rDownloading {:.1} of {:.1} MB",
                done as f64 / 1e6,
                total as f64 / 1e6
            );
            let _ = std::io::stdout().flush();
        } else if pct / 10 > shown.get() / 10 || done == total {
            println!("Downloading: {pct}%");
            shown.set(pct);
        }
    };
    let prepared = updater
        .prepare(&release, &dir, progress, || {
            if tty {
                println!();
            }
            println!("Checking the download and trying the new version…");
        })
        .await;
    let _ = std::fs::remove_dir_all(&dir);
    let top = match prepared {
        Ok(top) => top,
        Err(e) => {
            eprintln!("flux-server: the update failed, nothing changed: {e:#}");
            if let Some(staging) = s.install.staging_dir() {
                let _ = std::fs::remove_dir_all(staging);
            }
            return FAILED;
        }
    };
    if let Err(e) = put_in_place(&s.install, &top, &s.current.to_string()) {
        eprintln!("flux-server: putting the new version in place failed, nothing changed: {e:#}");
        return FAILED;
    }
    println!(
        "flux {} is in place (flux {} is kept for `flux-server update --rollback`).",
        release.version, s.current
    );
    restart_running(cfg, &s.current, &release.version).await
}

/// Asks a running flux (this data dir's, or one answering on its port as the
/// replaced version) to restart as the new version, and waits for it.
async fn restart_running(cfg: &Config, from: &Version, to: &Version) -> u8 {
    let client = super::http_client();
    let Some(url) = running(cfg, &client).await else {
        println!("Start flux to use {to}.");
        return OK;
    };
    if version_at(&client, &url).await.as_ref() != Some(from) {
        println!("Restart flux to use {to}.");
        return OK;
    }
    let asked = client
        .post(format!("{url}/api/v1/admin/restart"))
        .header(flux_core::api::REQUEST_HEADER, "1")
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    match asked {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            println!(
                "The running flux didn't restart ({}): restart it yourself to use {to}.",
                r.status()
            );
            return OK;
        }
        Err(e) => {
            println!("Couldn't ask the running flux to restart ({e}): restart it yourself to use {to}.");
            return OK;
        }
    }
    print!("Restarting flux…");
    let _ = std::io::stdout().flush();
    for _ in 0..240 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if version_at(&client, &url).await.as_ref() == Some(to) {
            println!(" running {to}.");
            return OK;
        }
    }
    println!();
    eprintln!("flux didn't come back as {to} within a minute: see its log.");
    FAILED
}

/// The address of the flux that runs on this data dir (`server.json`), or
/// on the configured local port.
async fn running(cfg: &Config, client: &reqwest::Client) -> Option<String> {
    let from_file = std::fs::read(cfg.data_dir.join(SERVER_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["url"].as_str().map(str::to_string));
    let port = format!("http://127.0.0.1:{}", cfg.local_addr.port());
    for url in from_file.into_iter().chain([port]) {
        if version_at(client, &url).await.is_some() {
            return Some(url);
        }
    }
    None
}

async fn version_at(client: &reqwest::Client, url: &str) -> Option<Version> {
    let info: flux_core::api::ServerInfo = client
        .get(format!("{url}/api/v1/server"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Version::parse(&info.version).ok()
}

/// Puts back the version the last update replaced. flux must be stopped: a
/// catalog the newer version migrated is restored from the copy it made
/// first, which the older version can open.
async fn rollback(cfg: &Config, updater: &Updater, yes: bool) -> u8 {
    let s = updater.settings();
    let (Some(root), Some(previous)) = (s.install.root.as_deref(), s.install.previous_dir()) else {
        eprintln!("Only a release package unpacked by hand keeps the version an update replaced.");
        return NOT_HERE;
    };
    let Some(kept) = updater.previous_version() else {
        eprintln!("No earlier version is kept in {}.", previous.display());
        return FAILED;
    };
    let client = super::http_client();
    if let Some(url) = running(cfg, &client).await {
        eprintln!("flux is running ({url}): stop it first, then run this again.");
        return FAILED;
    }
    let since = std::fs::metadata(previous.join(package::PREVIOUS_VERSION))
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let catalog = cfg.catalog_path();
    // The copy the newer version made before its first migration, if it migrated.
    let restore = flux_catalog::schema::backups(&catalog)
        .into_iter()
        .filter(|(_, p)| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t >= since)
        })
        .min_by_key(|(v, _)| *v);
    println!("This puts flux {kept} back in place of {}.", s.current);
    if let Some((schema, path)) = &restore {
        println!(
            "flux {} changed the catalog's format, which {kept} can't read. Its copy from before that \
             (format {schema}, {}) goes back too: what changed since is lost (scans, edits, watched state).",
            s.current,
            path.display()
        );
    }
    if !yes && !confirm("Go back?", false) {
        println!("Nothing changed.");
        return OK;
    }
    if let Some((_, path)) = &restore {
        if let Err(e) = restore_catalog(path, &catalog) {
            eprintln!("flux-server: couldn't put the catalog back, nothing changed: {e:#}");
            return FAILED;
        }
    }
    match package::roll_back(root, &previous, s.install.layout) {
        Ok(_) => {
            let _ = std::fs::write(
                previous.join(package::PREVIOUS_VERSION),
                format!("{}\n", s.current),
            );
            println!("flux {kept} is back. Start flux to use it.");
            OK
        }
        Err(e) => {
            eprintln!("flux-server: going back failed: {e:#}");
            FAILED
        }
    }
}

/// Copies `backup` over `catalog`, keeping the replaced catalog beside it
/// (`catalog.db.rolled-back`), and drops its WAL files.
fn restore_catalog(backup: &Path, catalog: &Path) -> anyhow::Result<()> {
    let aside = catalog.with_extension("db.rolled-back");
    if catalog.exists() {
        std::fs::copy(catalog, &aside)?;
    }
    let tmp = catalog.with_extension("db.restoring");
    std::fs::copy(backup, &tmp)?;
    for side in ["db-wal", "db-shm"] {
        let _ = std::fs::remove_file(catalog.with_extension(side));
    }
    std::fs::rename(&tmp, catalog)?;
    Ok(())
}

/// Asks on a terminal; `default` when Enter is pressed. Not a terminal:
/// no (use --yes).
fn confirm(question: &str, default: bool) -> bool {
    if !std::io::stdin().is_terminal() {
        eprintln!("{question} Not asked: this isn't a terminal. Pass --yes to go ahead.");
        return false;
    }
    print!("{question} [{}] ", if default { "Y/n" } else { "y/N" });
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    }
}
