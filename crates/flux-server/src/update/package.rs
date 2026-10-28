//! A new version's package, from download to in place: fetched and checked
//! against `SHA256SUMS`, unpacked beside the install, tried (`--version`),
//! then swapped in entry by entry, with what it replaces kept for rollback.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use semver::Version;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::install::{Layout, PACKAGE_ENTRIES};

/// The file in [`super::install::PREVIOUS_DIR`] that says which version is kept there.
pub const PREVIOUS_VERSION: &str = "version";

/// Downloads `url` to `dest` (through `dest.part`), checking its size and
/// SHA-256 on the way; `progress` hears the bytes so far.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    size: u64,
    sha256: &[u8; 32],
    mut progress: impl FnMut(u64),
) -> anyhow::Result<()> {
    let part = dest.with_extension("part");
    let mut res = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("downloading {url}"))?;
    let mut file = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut hash = Sha256::new();
    let mut done = 0u64;
    while let Some(chunk) = res.chunk().await.with_context(|| format!("downloading {url}"))? {
        done += chunk.len() as u64;
        ensure!(
            done <= size,
            "the download is larger than the release says ({size} bytes)"
        );
        hash.update(&chunk);
        file.write_all(&chunk).await?;
        progress(done);
    }
    file.flush().await?;
    drop(file);
    ensure!(done == size, "the download stopped at {done} of {size} bytes");
    let got: [u8; 32] = hash.finalize().into();
    if &got != sha256 {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("the download doesn't match SHA256SUMS: it was changed or damaged on the way");
    }
    tokio::fs::rename(&part, dest).await?;
    Ok(())
}

/// Unpacks a package (`.tar.gz` or `.zip`, by name) into `into`, which must
/// be empty or missing, and returns its top folder: a package is one folder
/// of plain files and folders.
pub fn unpack(archive: &Path, into: &Path) -> anyhow::Result<PathBuf> {
    if into.exists() {
        std::fs::remove_dir_all(into).with_context(|| format!("clearing {}", into.display()))?;
    }
    std::fs::create_dir_all(into)?;
    let name = archive.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name.ends_with(".zip") {
        let bytes = std::fs::read(archive)?;
        super::unzip::unzip(&bytes, into)?;
    } else if name.ends_with(".tar.gz") {
        let gz = flate2::read::GzDecoder::new(std::fs::File::open(archive)?);
        let mut tar = tar::Archive::new(gz);
        tar.set_preserve_permissions(true);
        tar.set_overwrite(false);
        for entry in tar.entries()? {
            let mut entry = entry?;
            let kind = entry.header().entry_type();
            let path = entry.path()?.into_owned();
            ensure!(
                kind.is_file() || kind.is_dir(),
                "{}: {} is a {kind:?}: a flux package holds only files and folders",
                archive.display(),
                path.display()
            );
            // `unpack_in` refuses paths that would leave `into`.
            ensure!(
                entry.unpack_in(into)?,
                "{}: {} would land outside the folder",
                archive.display(),
                path.display()
            );
        }
    } else {
        bail!("{name}: not a flux package");
    }
    let mut tops = std::fs::read_dir(into)?.flatten().map(|e| e.path());
    match (tops.next(), tops.next()) {
        (Some(top), None) if top.is_dir() => Ok(top),
        _ => bail!("{}: a flux package is one folder", archive.display()),
    }
}

/// How long the new binary may take to say its version (antivirus scans of
/// a new .exe can be slow).
const TRY_RUN_FOR: Duration = Duration::from_secs(60);

/// Runs `exe --version`: the new version must start on this machine (the
/// right architecture, a new enough OS and C library) and be `version`.
pub fn try_run(exe: &Path, version: &Version) -> anyhow::Result<()> {
    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("the new flux-server doesn't start here ({})", exe.display()))?;
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > TRY_RUN_FOR {
            let _ = child.kill();
            bail!(
                "the new flux-server didn't answer `--version` within {} s",
                TRY_RUN_FOR.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut o, &mut out);
    }
    ensure!(
        status.success(),
        "the new flux-server fails to start here ({status})"
    );
    let said = out.trim();
    ensure!(
        said.rsplit(' ').next() == Some(version.to_string().as_str()),
        "the new flux-server says {said:?}, not version {version}"
    );
    Ok(())
}

/// The top-level entries to move: everything the new package has, and the
/// package entries ([`PACKAGE_ENTRIES`]) at `root` that it doesn't.
fn entries_to_move(root: &Path, new_top: &Path) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(new_top)?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    for known in PACKAGE_ENTRIES {
        if !names.iter().any(|n| n == known) && root.join(known).symlink_metadata().is_ok() {
            names.push((*known).to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Puts the package at `new_top` in place at `root`, one top-level entry at
/// a time (the folder itself stays, with its owner and permissions, and so
/// does anything in it that isn't part of a package). What it replaces moves
/// to `previous`, emptied first. A failed move undoes the ones before it.
///
/// Entry by entry because Windows won't rename a folder holding a running
/// program, though it renames the program itself.
pub fn swap_in(root: &Path, new_top: &Path, previous: &Path) -> anyhow::Result<()> {
    if previous.exists() {
        std::fs::remove_dir_all(previous).with_context(|| format!("clearing {}", previous.display()))?;
    }
    std::fs::create_dir_all(previous)?;
    let names = entries_to_move(root, new_top)?;
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    if let Err(e) = move_all(root, new_top, previous, &names, &mut done) {
        for (from, to) in done.iter().rev() {
            if let Err(undo) = std::fs::rename(to, from) {
                tracing::error!(error = %undo, from = %to.display(), to = %from.display(), "couldn't undo a move of the update");
            }
        }
        return Err(e);
    }
    Ok(())
}

/// The moves of [`swap_in`], each recorded in `done` once made.
fn move_all(
    root: &Path,
    new_top: &Path,
    previous: &Path,
    names: &[String],
    done: &mut Vec<(PathBuf, PathBuf)>,
) -> anyhow::Result<()> {
    // All the old entries out first, then the new ones in.
    let out = names.iter().map(|n| (root.join(n), previous.join(n)));
    let into = names.iter().map(|n| (new_top.join(n), root.join(n)));
    for (from, to) in out.chain(into) {
        if from.symlink_metadata().is_err() {
            continue;
        }
        std::fs::rename(&from, &to)
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        done.push((from, to));
    }
    Ok(())
}

/// Puts the version kept in `previous` back at `root`; the one it replaces
/// is kept there in turn (so the same call goes forward again).
pub fn roll_back(root: &Path, previous: &Path, layout: Layout) -> anyhow::Result<Option<String>> {
    ensure!(
        previous.join(layout.exe()).is_file(),
        "no earlier version is kept in {}",
        previous.display()
    );
    let kept = std::fs::read_to_string(previous.join(PREVIOUS_VERSION))
        .ok()
        .map(|s| s.trim().to_string());
    // The kept version's own note isn't part of the package.
    let _ = std::fs::remove_file(previous.join(PREVIOUS_VERSION));
    let swap = previous.with_file_name(format!(
        "{}.swap",
        previous
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("previous")
    ));
    swap_in(root, previous, &swap)?;
    std::fs::remove_dir_all(previous)?;
    std::fs::rename(&swap, previous)?;
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(top: &Path, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let p = top.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_else(|_| "<missing>".into())
    }

    #[test]
    fn swaps_a_package_in_and_keeps_what_isnt_flux() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("flux");
        tree(
            &root,
            &[
                ("bin/flux-server", "old"),
                ("share/flux/web/index.html", "old web"),
                ("LICENSE", "old licence"),
                ("README.md", "old"),
                ("ffmpeg.exe", "mine"),
                ("notes.txt", "mine too"),
            ],
        );
        let new = d.path().join("staging/flux-2.0.0-x");
        tree(
            &new,
            &[
                ("bin/flux-server", "new"),
                ("share/flux/web/index.html", "new web"),
                ("LICENSE", "new licence"),
                ("install-kind", "archive"),
            ],
        );
        let prev = root.join(".flux-previous");
        swap_in(&root, &new, &prev).unwrap();
        assert_eq!(read(&root.join("bin/flux-server")), "new");
        assert_eq!(read(&root.join("share/flux/web/index.html")), "new web");
        assert_eq!(read(&root.join("install-kind")), "archive");
        // README.md was the old package's and isn't in the new one: kept aside, not left behind.
        assert_eq!(read(&root.join("README.md")), "<missing>");
        assert_eq!(read(&prev.join("README.md")), "old");
        assert_eq!(read(&prev.join("bin/flux-server")), "old");
        // Not flux's: untouched.
        assert_eq!(read(&root.join("ffmpeg.exe")), "mine");
        assert_eq!(read(&root.join("notes.txt")), "mine too");
    }

    #[test]
    fn rolls_back_and_forward() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("flux");
        tree(&root, &[("bin/flux-server", "1")]);
        let new = d.path().join("staging/flux-2");
        tree(&new, &[("bin/flux-server", "2")]);
        let prev = root.join(".flux-previous");
        swap_in(&root, &new, &prev).unwrap();
        std::fs::write(prev.join(PREVIOUS_VERSION), "1.0.0\n").unwrap();
        assert_eq!(
            roll_back(&root, &prev, Layout::Unix).unwrap().as_deref(),
            Some("1.0.0")
        );
        assert_eq!(read(&root.join("bin/flux-server")), "1");
        assert_eq!(read(&prev.join("bin/flux-server")), "2");
        assert_eq!(roll_back(&root, &prev, Layout::Unix).unwrap(), None);
        assert_eq!(read(&root.join("bin/flux-server")), "2");
        // Nothing kept: nothing to go back to.
        std::fs::remove_dir_all(&prev).unwrap();
        assert!(roll_back(&root, &prev, Layout::Unix).is_err());
        assert_eq!(read(&root.join("bin/flux-server")), "2");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_move_undoes_the_others() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("flux");
        tree(&root, &[("bin/flux-server", "old"), ("LICENSE", "old")]);
        let new = d.path().join("staging/flux-2");
        tree(&new, &[("bin/flux-server", "new"), ("LICENSE", "new")]);
        // The new package's folder can't give up its entries: moving them in fails.
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o555)).unwrap();
        let prev = root.join(".flux-previous");
        let res = swap_in(&root, &new, &prev);
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).unwrap();
        if res.is_ok() {
            return; // running as root: permissions don't stop it
        }
        assert_eq!(read(&root.join("bin/flux-server")), "old");
        assert_eq!(read(&root.join("LICENSE")), "old");
        assert_eq!(read(&new.join("bin/flux-server")), "new");
    }

    fn tar_gz(top: &str, files: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (rel, body, mode) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(*mode);
            h.set_cksum();
            b.append_data(&mut h, format!("{top}/{rel}"), *body).unwrap();
        }
        let raw = b.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &raw).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn unpacks_a_tarball_into_its_one_folder() {
        let d = tempfile::tempdir().unwrap();
        let archive = d.path().join("flux-2.0.0-aarch64-apple-darwin.tar.gz");
        std::fs::write(
            &archive,
            tar_gz(
                "flux-2.0.0-aarch64-apple-darwin",
                &[
                    ("bin/flux-server", b"#!/bin/sh\n", 0o755),
                    ("share/flux/install-kind", b"archive\n", 0o644),
                ],
            ),
        )
        .unwrap();
        let into = d.path().join("staging");
        let top = unpack(&archive, &into).unwrap();
        assert_eq!(top, into.join("flux-2.0.0-aarch64-apple-darwin"));
        assert_eq!(read(&top.join("share/flux/install-kind")), "archive\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(top.join("bin/flux-server"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "still executable");
        }
    }

    #[test]
    fn refuses_a_tarball_that_isnt_one_folder_of_files() {
        let d = tempfile::tempdir().unwrap();
        let two = d.path().join("two.tar.gz");
        let mut b = tar::Builder::new(Vec::new());
        for name in ["a/x", "b/y"] {
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, &b"z"[..]).unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &b.into_inner().unwrap()).unwrap();
        std::fs::write(&two, gz.finish().unwrap()).unwrap();
        assert!(unpack(&two, &d.path().join("s1")).is_err());

        // A link, even one pointing inside.
        let link = d.path().join("link.tar.gz");
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        b.append_link(&mut h, "top/bin", "/usr/bin").unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &b.into_inner().unwrap()).unwrap();
        std::fs::write(&link, gz.finish().unwrap()).unwrap();
        assert!(unpack(&link, &d.path().join("s2")).is_err());

        assert!(unpack(&d.path().join("flux.rar"), &d.path().join("s3")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn tries_the_new_binary_before_anything_moves() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("flux-server");
        std::fs::write(&exe, "#!/bin/sh\necho \"flux-server 2.0.0\"\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        try_run(&exe, &Version::parse("2.0.0").unwrap()).unwrap();
        let err = try_run(&exe, &Version::parse("2.0.1").unwrap()).unwrap_err();
        assert!(err.to_string().contains("not version 2.0.1"), "{err}");
        std::fs::write(&exe, "#!/bin/sh\nexit 3\n").unwrap();
        assert!(try_run(&exe, &Version::parse("2.0.0").unwrap()).is_err());
        assert!(try_run(&d.path().join("missing"), &Version::parse("2.0.0").unwrap()).is_err());
    }
}
