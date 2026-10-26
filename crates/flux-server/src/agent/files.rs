//! Organising files (docs/AGENT-NATIVE.md, "Organising files"):
//! `files.plan` works out how to name items' files by a scheme (or where to
//! move files an agent names) and keeps it as a proposed changeset; nothing
//! moves until it's applied (`changes.apply`), and `changes.undo` puts
//! everything back.
//!
//! A plan is a list of steps ([`organize::Step`]): make a folder, rename a
//! folder (everything in it comes along), move a video or a file named like
//! it (subtitles, a metadata file, an image), remove a folder the plan
//! empties. Steps run in order and are undone in reverse. Every step stays
//! inside its library folder, never replaces anything (the platform's
//! no-replace rename: [`rename_noreplace`]), never crosses a drive, never
//! goes through a link, and is checked again right before it runs.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use flux_catalog::organize::{self, Done, Role, Step, VideoFile, Work};
use flux_core::agent::*;
use flux_core::api::Id;
use flux_core::LibraryKind;

use super::{announce, cat_err, err, Ctx};

/// One plan moves files at a time.
static MOVING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---- names ---------------------------------------------------------------------------------

/// `s` as a folder or file name every system flux runs on accepts: no
/// `/\:*?"<>|` or control characters, no leading dot, no trailing dots or
/// spaces, not a name Windows reserves, at most 180 bytes.
pub fn clean_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            ':' => out.push_str(" - "),
            '/' | '\\' | '|' | '*' => out.push('-'),
            '?' => {}
            '"' => out.push('\''),
            '<' => out.push('('),
            '>' => out.push(')'),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    let mut out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    out = out.replace(" - - ", " - ");
    let mut out = out
        .trim_start_matches(['.', ' ', '-'])
        .trim_end_matches(['.', ' '])
        .to_string();
    if out.len() > 180 {
        let mut end = 180;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out = out.trim_end_matches(['.', ' ']).to_string();
    }
    let upper = out.to_ascii_uppercase();
    let base = upper.split('.').next().unwrap_or("");
    let reserved = matches!(base, "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit());
    if reserved {
        out.push('_');
    }
    out
}

/// A work's folder name: `Title (Year)`, with an id when asked.
fn work_name(w: &Work, title: &str, ids: bool) -> String {
    let mut n = clean_name(title);
    if n.is_empty() {
        n = "Untitled".into();
    }
    if let Some(y) = w.year {
        n = format!("{n} ({y})");
    }
    if ids {
        if let Some(i) = &w.imdb {
            n.push_str(&format!(" [imdbid-{i}]"));
        } else if let Some(t) = w.tmdb {
            n.push_str(&format!(" [tmdbid-{t}]"));
        } else if let Some(t) = w.tvdb {
            n.push_str(&format!(" [tvdbid-{t}]"));
        }
    }
    n
}

/// The base of a film's or episode's file name: its folder name without
/// the id.
fn file_base(w: &Work, title: &str) -> String {
    let mut n = clean_name(title);
    if n.is_empty() {
        n = "Untitled".into();
    }
    match w.year {
        Some(y) => format!("{n} ({y})"),
        None => n,
    }
}

fn height_label(h: u32) -> &'static str {
    match h {
        h if h >= 1800 => "2160p",
        h if h >= 1000 => "1080p",
        h if h >= 700 => "720p",
        h if h >= 560 => "576p",
        _ => "480p",
    }
}

fn season_folder(season: u16) -> String {
    if season == 0 {
        "Specials".into()
    } else {
        format!("Season {season:02}")
    }
}

/// The folder an extra of `kind` goes in (names Plex, Jellyfin and Kodi
/// read).
fn extras_folder(kind: Option<&str>) -> &'static str {
    match kind {
        Some("trailer") => "Trailers",
        Some("featurette") => "Featurettes",
        Some("behind_the_scenes") => "Behind The Scenes",
        Some("deleted_scene") => "Deleted Scenes",
        Some("interview") => "Interviews",
        Some("short") => "Shorts",
        Some("scene") => "Scenes",
        _ => "Other",
    }
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

fn name_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, n)| n)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// A file name's extension, with its dot (`.mkv`), lowercased.
fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, e)| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default()
}

fn stem_of(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// Check a path an agent gave (`files.plan` moves): relative, `/` between
/// names, each name one [`clean_name`] keeps.
fn check_rel(path: &str) -> Result<String, String> {
    let p = path.trim().trim_matches('/');
    if p.is_empty() {
        return Err("the path is empty".into());
    }
    let drive = p.as_bytes().get(1) == Some(&b':');
    if p.contains('\\') || p.starts_with('/') || drive {
        return Err(format!(
            "{path:?}: give a path relative to the library folder, with / between folders"
        ));
    }
    for part in p.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("{path:?}: no empty, . or .. parts"));
        }
        if clean_name(part) != part {
            return Err(format!(
                "{path:?}: {part:?} isn't a name every system accepts (try {:?})",
                clean_name(part)
            ));
        }
    }
    Ok(p.to_string())
}

// ---- the disk ------------------------------------------------------------------------------

/// The absolute path of `rel` in a library folder.
fn abs(root: &str, rel: &str) -> PathBuf {
    let mut p = PathBuf::from(root);
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        p.push(part);
    }
    p
}

fn exists(root: &str, rel: &str) -> bool {
    std::fs::symlink_metadata(abs(root, rel)).is_ok()
}

/// Whether a link stands between the library folder and `rel` (a step
/// never goes through one: it could lead out of the library).
fn behind_link(root: &str, rel: &str) -> bool {
    let mut p = PathBuf::from(root);
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        p.push(part);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

/// The entries of a folder (names), or none if it can't be listed.
fn list(root: &str, rel: &str) -> Vec<(String, bool)> {
    match std::fs::read_dir(abs(root, rel)) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let dir = e.file_type().ok()?.is_dir();
                Some((e.file_name().to_string_lossy().into_owned(), dir))
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Rename `from` to `to` unless something is already at `to`: an error
/// (`AlreadyExists`) rather than a replace. Never copies: across drives it
/// fails.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    imp::rename_noreplace(from, to)
}

#[cfg(target_os = "macos")]
// The platform's no-replace rename: one system call.
#[allow(unsafe_code)]
mod imp {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let f = CString::new(from.as_os_str().as_bytes())?;
        let t = CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: two valid NUL-terminated paths.
        let r = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
        if r == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(target_os = "linux")]
// The platform's no-replace rename: one system call.
#[allow(unsafe_code)]
mod imp {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const RENAME_NOREPLACE: libc::c_uint = 1;

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let f = CString::new(from.as_os_str().as_bytes())?;
        let t = CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: two valid NUL-terminated paths; renameat2 takes no buffers.
        let r = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                f.as_ptr(),
                libc::AT_FDCWD,
                t.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            // A filesystem (or kernel) without it: a file is linked (which
            // fails on anything there) then unlinked; a folder is checked,
            // then renamed.
            Some(libc::EINVAL) | Some(libc::ENOSYS) => {
                if std::fs::symlink_metadata(to).is_ok() {
                    return Err(io::Error::from(io::ErrorKind::AlreadyExists));
                }
                if std::fs::symlink_metadata(from)?.is_dir() {
                    std::fs::rename(from, to)
                } else {
                    std::fs::hard_link(from, to)?;
                    std::fs::remove_file(from)
                }
            }
            _ => Err(e),
        }
    }
}

#[cfg(windows)]
// The platform's no-replace rename: one system call.
#[allow(unsafe_code)]
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    /// `\\?\` form (long paths, no reinterpretation) of an absolute path.
    fn verbatim(p: &Path) -> Vec<u16> {
        let s = p.as_os_str().to_string_lossy().replace('/', "\\");
        let v = if s.starts_with(r"\\?\") {
            s
        } else if let Some(unc) = s.strip_prefix(r"\\") {
            format!(r"\\?\UNC\{unc}")
        } else {
            format!(r"\\?\{s}")
        };
        std::ffi::OsStr::new(&v).encode_wide().chain(Some(0)).collect()
    }

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (verbatim(from), verbatim(to));
        // SAFETY: two valid NUL-terminated wide paths. No
        // MOVEFILE_REPLACE_EXISTING (fails if `to` exists) and no
        // MOVEFILE_COPY_ALLOWED (fails across volumes).
        let ok = unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(f.as_ptr(), t.as_ptr(), 0) };
        if ok != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod imp {
    use std::io;
    use std::path::Path;

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        if std::fs::symlink_metadata(to).is_ok() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        std::fs::rename(from, to)
    }
}

/// The OS error for "on another drive" (`EXDEV`, `ERROR_NOT_SAME_DEVICE`).
#[cfg(unix)]
const OTHER_DRIVE: i32 = libc::EXDEV;
#[cfg(windows)]
const OTHER_DRIVE: i32 = 17;
#[cfg(not(any(unix, windows)))]
const OTHER_DRIVE: i32 = -1;

/// What went wrong with a step, in words.
fn io_reason(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::AlreadyExists => "something is already there".into(),
        io::ErrorKind::NotFound => "it isn't there any more".into(),
        io::ErrorKind::PermissionDenied => {
            "flux may not change it (permissions, or it is open elsewhere)".into()
        }
        _ if e.raw_os_error() == Some(OTHER_DRIVE) => "that would cross to another drive".into(),
        // ERROR_SHARING_VIOLATION
        _ if cfg!(windows) && e.raw_os_error() == Some(32) => "another program has it open".into(),
        _ => format!("{e}"),
    }
}

// ---- planning -----------------------------------------------------------------------------

/// A plan being worked out.
struct Planner {
    roots: BTreeMap<Id, organize::Root>,
    hide: Vec<String>,
    ids: bool,
    steps: Vec<Step>,
    skipped: Vec<PlanSkip>,
    /// Paths (lowercased) this plan puts something at.
    taken: BTreeSet<(Id, String)>,
    /// Folders this plan makes.
    made: BTreeSet<(Id, String)>,
    /// Folders renamed so far, in order: `(root, old, new)`.
    renamed: Vec<(Id, String, String)>,
    /// What moves out of each folder (original paths), for removing folders
    /// left empty: `(root, folder) → names`.
    leaving: BTreeMap<(Id, String), BTreeSet<String>>,
}

/// The catalog's facts a plan needs, read before it's worked out.
struct Facts {
    works: Vec<Work>,
    /// For explicit moves: the file and its work.
    files: Vec<(FileMove, Option<(VideoFile, Work)>)>,
    /// Folders that hold another work's files: `(root, folder, work) → others`.
    others: BTreeMap<(Id, String, Id), Vec<Id>>,
    roots: BTreeMap<Id, organize::Root>,
}

impl Planner {
    fn root(&self, id: Id) -> &organize::Root {
        &self.roots[&id]
    }

    /// Where `rel` (as it is now) will be when this step runs.
    fn now(&self, root: Id, rel: &str) -> String {
        let mut p = rel.to_string();
        for (r, old, new) in &self.renamed {
            if *r != root {
                continue;
            }
            if p == *old {
                p = new.clone();
            } else if let Some(rest) = p.strip_prefix(&format!("{old}/")) {
                p = format!("{new}/{rest}");
            }
        }
        p
    }

    /// Where `rel` (a path once this plan's folder renames are done) is on
    /// the disk now.
    fn disk(&self, root: Id, rel: &str) -> String {
        let mut p = rel.to_string();
        for (r, old, new) in self.renamed.iter().rev() {
            if *r != root {
                continue;
            }
            if p == *new {
                p = old.clone();
            } else if let Some(rest) = p.strip_prefix(&format!("{new}/")) {
                p = format!("{old}/{rest}");
            }
        }
        p
    }

    /// Whether something is at `rel` (as paths will be when the step runs),
    /// on the disk or in this plan.
    fn is_taken(&self, root: Id, rel: &str) -> bool {
        self.taken.contains(&(root, rel.to_lowercase()))
            || exists(&self.root(root).path, &self.disk(root, rel))
    }

    fn skip(&mut self, file: Option<Id>, item: Option<Id>, path: Option<&str>, reason: impl Into<String>) {
        self.skipped.push(PlanSkip {
            file,
            item,
            path: path.map(str::to_string),
            reason: reason.into(),
        });
    }

    /// Make folder `rel` (and the folders above it the plan doesn't have).
    fn make_dir(&mut self, root: Id, item: Id, rel: &str) {
        if rel.is_empty() {
            return;
        }
        let parent = dir_of(rel).to_string();
        self.make_dir(root, item, &parent);
        let key = (root, rel.to_lowercase());
        // A folder a rename of this plan makes is there too.
        if self.made.contains(&key) || exists(&self.root(root).path, &self.disk(root, rel)) {
            return;
        }
        self.made.insert(key.clone());
        self.taken.insert(key);
        self.steps.push(Step {
            field: organize::NEW_FOLDER.into(),
            item,
            root,
            from: None,
            to: Some(rel.to_string()),
            file: None,
            size: None,
            mtime_ns: None,
        });
    }

    fn rename_dir(&mut self, root: Id, item: Id, from: &str, to: &str) {
        self.taken.insert((root, to.to_lowercase()));
        self.steps.push(Step {
            field: organize::RENAME_FOLDER.into(),
            item,
            root,
            from: Some(from.to_string()),
            to: Some(to.to_string()),
            file: None,
            size: None,
            mtime_ns: None,
        });
        self.renamed.push((root, from.to_string(), to.to_string()));
    }

    /// A free name in `dir` for `base` + `ext`: `base`, else `base - 2`…
    /// (`current`: where the file will be when its step runs; its own path,
    /// or its path in another case, is free for it).
    fn free(&self, root: Id, dir: &str, base: &str, ext: &str, current: &str) -> Option<String> {
        for n in 1..=9 {
            let name = if n == 1 {
                format!("{base}{ext}")
            } else {
                format!("{base} - {n}{ext}")
            };
            let p = join(dir, &name);
            if p.to_lowercase() == current.to_lowercase() || !self.is_taken(root, &p) {
                return Some(p);
            }
        }
        None
    }

    /// Move a video (from its path now, `from`) to `to`, and the files named
    /// like it beside it.
    fn move_video(&mut self, v: &VideoFile, from: &str, to: &str) {
        let root = v.root;
        if from == to {
            return;
        }
        self.taken.insert((root, to.to_lowercase()));
        self.steps.push(Step {
            field: organize::MOVE_FILE.into(),
            item: v.item,
            root,
            from: Some(from.to_string()),
            to: Some(to.to_string()),
            file: Some(v.file),
            size: Some(v.size),
            mtime_ns: Some(v.mtime_ns),
        });
        self.leaving
            .entry((root, dir_of(&v.rel_path).to_string()))
            .or_default()
            .insert(name_of(&v.rel_path).to_string());
        // Files named like it (`Film.en.srt`, `Film.nfo`, `Film-poster.jpg`).
        let old_dir = dir_of(&v.rel_path).to_string();
        let old_stem = stem_of(name_of(&v.rel_path)).to_string();
        let new_dir = dir_of(to).to_string();
        let new_stem = stem_of(name_of(to)).to_string();
        let lower = old_stem.to_lowercase();
        let path = self.root(root).path.clone();
        // The folder as it is on the disk while planning.
        let mut names: Vec<(String, bool)> = list(&path, &old_dir);
        names.sort();
        for (name, is_dir) in names {
            if is_dir || name == name_of(&v.rel_path) {
                continue;
            }
            let nl = name.to_lowercase();
            let rest = if nl.starts_with(&format!("{lower}.")) || nl.starts_with(&format!("{lower}-")) {
                &name[old_stem.len()..]
            } else {
                continue;
            };
            if is_video_name(&name) {
                continue;
            }
            let c_from = join(&self.now(root, &old_dir), &name);
            let c_to = join(&new_dir, &format!("{new_stem}{rest}"));
            if c_from == c_to {
                continue;
            }
            if self.is_taken(root, &c_to) && c_to.to_lowercase() != c_from.to_lowercase() {
                self.skip(
                    None,
                    Some(v.item),
                    Some(&c_from),
                    format!("{c_to} is already there"),
                );
                continue;
            }
            self.taken.insert((root, c_to.to_lowercase()));
            self.steps.push(Step {
                field: organize::MOVE_FILE.into(),
                item: v.item,
                root,
                from: Some(c_from),
                to: Some(c_to),
                file: None,
                size: None,
                mtime_ns: None,
            });
            self.leaving
                .entry((root, old_dir.clone()))
                .or_default()
                .insert(name);
        }
    }

    /// Whether `dir` (as it is now, original path) holds files of no other
    /// work than `work`.
    fn own(&self, facts: &Facts, root: Id, dir: &str, work: Id) -> bool {
        !dir.is_empty()
            && facts
                .others
                .get(&(root, dir.to_string(), work))
                .is_none_or(|o| o.is_empty())
    }

    /// Name a work's files by the standard scheme.
    fn work(&mut self, facts: &Facts, w: &Work) {
        if w.library_kind == "videos" {
            self.skip(
                None,
                Some(w.id),
                None,
                "a videos library keeps your own names: move its files where you want them (moves)",
            );
            return;
        }
        if w.title_provider.as_ref().is_some_and(|p| self.hide.contains(p)) {
            let p = w.title_provider.clone().unwrap_or_default();
            self.skip(
                None,
                Some(w.id),
                None,
                format!("its title is {p}'s, which agents may not see: rename it in the app, or set its title first"),
            );
            return;
        }
        let present: Vec<&VideoFile> = w.files.iter().filter(|f| f.present).collect();
        for f in w.files.iter().filter(|f| !f.present) {
            self.skip(
                Some(f.file),
                Some(f.item),
                Some(&f.rel_path),
                "its drive doesn't answer, or the file is gone",
            );
        }
        let Some(first) = present.first() else {
            return;
        };
        let root = first.root;
        if !self.root(root).available {
            self.skip(None, Some(w.id), None, "its library folder doesn't answer");
            return;
        }
        let files: Vec<&VideoFile> = present
            .into_iter()
            .filter(|f| {
                if f.root == root {
                    return true;
                }
                false
            })
            .collect();
        for f in w.files.iter().filter(|f| f.present && f.root != root) {
            self.skip(
                Some(f.file),
                Some(f.item),
                Some(&f.rel_path),
                "it's in another library folder than the rest of its work",
            );
        }
        let path = self.root(root).path.clone();
        for f in &files {
            if behind_link(&path, &f.rel_path) {
                self.skip(
                    Some(f.file),
                    Some(f.item),
                    Some(&f.rel_path),
                    "it's behind a link",
                );
                return;
            }
        }
        let folder = work_name(w, &w.title, self.ids);
        let base = file_base(w, &w.title);
        // The work's own folder: the one its main files are in, holding no
        // other work's files.
        let mains: Vec<&&VideoFile> = files
            .iter()
            .filter(|f| !matches!(f.role, Role::Extra { .. }))
            .collect();
        let main_dirs: BTreeSet<String> = mains
            .iter()
            .map(|f| match f.role {
                Role::Episode { .. } => flux_catalog::metafiles::show_dir(&f.rel_path).unwrap_or_default(),
                _ => dir_of(&f.rel_path).to_string(),
            })
            .collect();
        let home = if main_dirs.len() == 1 {
            main_dirs.into_iter().next().unwrap_or_default()
        } else {
            String::new()
        };
        let target_dir = if self.own(facts, root, &home, w.id) {
            // Rename it in place (its parent stays).
            let want = join(dir_of(&home), &folder);
            if home != want {
                if home.to_lowercase() != want.to_lowercase() && self.is_taken(root, &want) {
                    self.skip(None, Some(w.id), Some(&home), format!("{want} is already there"));
                    return;
                }
                self.rename_dir(root, w.id, &home, &want);
            }
            want
        } else {
            // A folder of its own, beside where it is.
            let parent = if mains.is_empty() {
                dir_of(&files[0].rel_path).to_string()
            } else {
                common_dir(mains.iter().map(|f| match f.role {
                    Role::Episode { .. } => {
                        flux_catalog::metafiles::show_dir(&f.rel_path).unwrap_or_default()
                    }
                    _ => dir_of(&f.rel_path).to_string(),
                }))
            };
            let want = join(&parent, &folder);
            if exists(&path, &want) && !self.own(facts, root, &want, w.id) {
                self.skip(
                    None,
                    Some(w.id),
                    Some(&want),
                    format!("{want} holds another work's files"),
                );
                return;
            }
            self.make_dir(root, w.id, &want);
            want
        };
        // Versions of a film: tell them apart only when needed.
        let versions: Vec<&&VideoFile> = files
            .iter()
            .filter(|f| matches!(f.role, Role::Version { .. }))
            .collect();
        let plain = versions
            .iter()
            .filter(|f| {
                matches!(
                    f.role,
                    Role::Version {
                        edition: None,
                        part: None,
                        ..
                    }
                )
            })
            .count();
        let mut seasons_done: BTreeMap<u16, String> = BTreeMap::new();
        for f in &files {
            let from = self.now(root, &f.rel_path);
            let ext = ext_of(name_of(&f.rel_path));
            let (dir, stem) = match &f.role {
                Role::Version {
                    edition,
                    part,
                    height,
                } => {
                    let mut stem = base.clone();
                    if let Some(e) = edition {
                        stem.push_str(&format!(" - {}", clean_name(e)));
                    } else if plain > 1 {
                        if let Some(h) = height {
                            stem.push_str(&format!(" - {}", height_label(*h)));
                        }
                    }
                    if let Some(p) = part {
                        stem.push_str(&format!(" - part{p}"));
                    }
                    (target_dir.clone(), stem)
                }
                Role::Episode {
                    season,
                    episodes,
                    titles,
                    title_providers,
                    air_date,
                } => {
                    let sdir = match seasons_done.get(season) {
                        Some(d) => d.clone(),
                        None => {
                            let d = self.season_dir(facts, w, root, &target_dir, *season, &files);
                            seasons_done.insert(*season, d.clone());
                            d
                        }
                    };
                    let mut stem = base.clone();
                    if episodes.is_empty() {
                        if let Some(d) = air_date {
                            stem.push_str(&format!(" - {d}"));
                        }
                    } else {
                        stem.push_str(&format!(" - S{season:02}"));
                        for (i, e) in episodes.iter().enumerate() {
                            if i == 0 {
                                stem.push_str(&format!("E{e:02}"));
                            } else {
                                stem.push_str(&format!("-E{e:02}"));
                            }
                        }
                    }
                    let visible: Vec<&str> = titles
                        .iter()
                        .zip(title_providers)
                        .filter_map(|(t, p)| match p {
                            Some(p) if self.hide.contains(p) => None,
                            _ => t.as_deref(),
                        })
                        .collect();
                    if visible.len() == titles.len() && !visible.is_empty() {
                        let t = clean_name(&visible.join(" & "));
                        if !t.is_empty() {
                            stem.push_str(&format!(" - {t}"));
                        }
                    }
                    (sdir, stem)
                }
                Role::Extra { kind, title } => {
                    let d = join(&target_dir, extras_folder(kind.as_deref()));
                    self.make_dir(root, w.id, &d);
                    let t = clean_name(title);
                    (d, if t.is_empty() { "Extra".into() } else { t })
                }
            };
            let current = from.clone();
            let Some(to) = self.free(root, &dir, &stem, &ext, &current) else {
                self.skip(
                    Some(f.file),
                    Some(f.item),
                    Some(&f.rel_path),
                    format!("no free name for it in {dir}"),
                );
                continue;
            };
            self.move_video(f, &from, &to);
        }
    }

    /// The season folder of a show's season: an existing folder holding only
    /// that season's episodes is renamed; otherwise one is made.
    fn season_dir(
        &mut self,
        facts: &Facts,
        w: &Work,
        root: Id,
        show_dir: &str,
        season: u16,
        files: &[&VideoFile],
    ) -> String {
        let want = join(show_dir, &season_folder(season));
        let theirs: Vec<&&VideoFile> = files
            .iter()
            .filter(|f| matches!(&f.role, Role::Episode { season: s, .. } if *s == season))
            .collect();
        let dirs: BTreeSet<String> = theirs.iter().map(|f| dir_of(&f.rel_path).to_string()).collect();
        if dirs.len() == 1 {
            let d = dirs.into_iter().next().unwrap_or_default();
            let only_this_season = files.iter().all(|f| {
                dir_of(&f.rel_path) != d || matches!(&f.role, Role::Episode { season: s, .. } if *s == season)
            });
            let is_show_dir =
                flux_catalog::metafiles::show_dir(&theirs[0].rel_path).as_deref() == Some(d.as_str());
            if !d.is_empty() && !is_show_dir && only_this_season && self.own(facts, root, &d, w.id) {
                let now = self.now(root, &d);
                if now == want {
                    return want;
                }
                if now.to_lowercase() == want.to_lowercase() || !self.is_taken(root, &want) {
                    // Inside the show's folder, by its new name.
                    if dir_of(&now) == show_dir {
                        self.rename_dir(root, w.id, &now, &want);
                        return want;
                    }
                }
            }
        }
        self.make_dir(root, w.id, &want);
        want
    }

    /// A file an agent asked to move.
    fn explicit(&mut self, m: &FileMove, found: Option<&(VideoFile, Work)>) {
        let to = match check_rel(&m.to) {
            Ok(t) => t,
            Err(why) => {
                self.skip(Some(m.file), None, None, why);
                return;
            }
        };
        let Some((v, w)) = found else {
            self.skip(Some(m.file), None, None, "no such file in the catalog");
            return;
        };
        if !v.present {
            self.skip(
                Some(v.file),
                Some(v.item),
                Some(&v.rel_path),
                "its drive doesn't answer, or the file is gone",
            );
            return;
        }
        if w.title_provider.as_ref().is_some_and(|p| self.hide.contains(p)) {
            // The path it's at isn't the provider's: a move names nothing.
        }
        if ext_of(name_of(&to)) != ext_of(name_of(&v.rel_path)) {
            self.skip(
                Some(v.file),
                Some(v.item),
                Some(&v.rel_path),
                "keep its extension: a new name can't change what the file is",
            );
            return;
        }
        let path = self.root(v.root).path.clone();
        if behind_link(&path, &v.rel_path) || behind_link(&path, dir_of(&to)) {
            self.skip(
                Some(v.file),
                Some(v.item),
                Some(&v.rel_path),
                "it's behind a link",
            );
            return;
        }
        let from = self.now(v.root, &v.rel_path);
        if from == to {
            return;
        }
        if self.is_taken(v.root, &to) && to.to_lowercase() != from.to_lowercase() {
            self.skip(
                Some(v.file),
                Some(v.item),
                Some(&v.rel_path),
                format!("{to} is already there"),
            );
            return;
        }
        self.make_dir(v.root, w.id, dir_of(&to));
        self.move_video(v, &from, &to);
    }

    /// Folders the plan leaves empty go (deepest first).
    fn tidy(&mut self) {
        let mut leaving = std::mem::take(&mut self.leaving);
        loop {
            let mut gone: Vec<(Id, String)> = Vec::new();
            for ((root, dir), names) in &leaving {
                if dir.is_empty() {
                    continue;
                }
                let now = self.now(*root, dir);
                // A folder that was renamed (the work's own) stays.
                if self.renamed.iter().any(|(r, _, new)| r == root && new == &now) {
                    continue;
                }
                if self.taken.iter().any(|(r, p)| {
                    r == root
                        && (p == &now.to_lowercase() || p.starts_with(&format!("{}/", now.to_lowercase())))
                }) {
                    continue;
                }
                let path = &self.root(*root).path;
                let entries = list(path, dir);
                if !entries.is_empty() && entries.iter().all(|(n, _)| names.contains(n)) {
                    gone.push((*root, dir.clone()));
                }
            }
            if gone.is_empty() {
                break;
            }
            gone.sort_by_key(|(_, d)| std::cmp::Reverse(d.matches('/').count()));
            for (root, dir) in gone {
                leaving.remove(&(root, dir.clone()));
                let now = self.now(root, &dir);
                self.steps.push(Step {
                    field: organize::REMOVE_FOLDER.into(),
                    item: 0,
                    root,
                    from: Some(now),
                    to: None,
                    file: None,
                    size: None,
                    mtime_ns: None,
                });
                // Its parent loses a folder.
                let parent = dir_of(&dir).to_string();
                if !parent.is_empty() {
                    leaving
                        .entry((root, parent))
                        .or_default()
                        .insert(name_of(&dir).to_string());
                }
            }
        }
    }
}

/// The deepest folder all of `dirs` are in.
fn common_dir<'a>(dirs: impl Iterator<Item = String> + 'a) -> String {
    let mut common: Option<Vec<String>> = None;
    for d in dirs {
        let parts: Vec<String> = d
            .split('/')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        common = Some(match common {
            None => parts,
            Some(c) => c
                .into_iter()
                .zip(parts)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect(),
        });
    }
    common.unwrap_or_default().join("/")
}

fn is_video_name(name: &str) -> bool {
    flux_names::is_video_file(name)
}

/// Work out a plan (`files.plan`) and keep it as a proposal.
pub async fn plan(ctx: &Ctx, input: PlanInput) -> Result<PlanOutput, OpError> {
    if !ctx.may().propose {
        return Err(err(
            ErrorCode::Forbidden,
            "Your role can't change the library (Owners and Admins can; Viewers can propose)",
        ));
    }
    let summary = super::ops::summary_of_input(&input.summary)?;
    if input.items.is_empty() && input.moves.is_empty() {
        return Err(err(
            ErrorCode::Invalid,
            "Give items to name by the scheme, or moves",
        ));
    }
    if input.items.len() > 200 || input.moves.len() > 500 {
        return Err(err(
            ErrorCode::TooLarge,
            "One plan takes at most 200 items or 500 moves: plan in parts",
        ));
    }
    let (items, moves) = (input.items.clone(), input.moves.clone());
    let facts = ctx
        .st
        .catalog
        .read(move |c| {
            let mut works: Vec<Work> = Vec::new();
            let mut seen = BTreeSet::new();
            for i in items {
                let w = organize::work_files(c, i)?
                    .ok_or_else(|| flux_catalog::CatalogError::NotFound(format!("item {i}")))?;
                if seen.insert(w.id) {
                    works.push(w);
                } else if let Some(prev) = works.iter_mut().find(|x| x.id == w.id) {
                    for f in w.files {
                        if !prev.files.iter().any(|p| p.file == f.file) {
                            prev.files.push(f);
                        }
                    }
                }
            }
            let mut files = Vec::new();
            for m in moves {
                let found = match c
                    .prepare_cached(
                        "SELECT a.item_id FROM file f JOIN asset a ON a.id = f.asset_id WHERE f.id = ?1",
                    )?
                    .query_row([m.file], |r| r.get::<_, Id>(0))
                {
                    Ok(item) => organize::work_files(c, item)?.and_then(|w| {
                        let v = w.files.iter().find(|v| v.file == m.file).cloned()?;
                        Some((v, w))
                    }),
                    Err(_) => None,
                };
                files.push((m, found));
            }
            let mut roots = BTreeMap::new();
            let mut others = BTreeMap::new();
            let mut dirs: BTreeSet<(Id, String, Id)> = BTreeSet::new();
            for w in &works {
                for f in &w.files {
                    let mut d = dir_of(&f.rel_path).to_string();
                    while !d.is_empty() {
                        dirs.insert((f.root, d.clone(), w.id));
                        d = dir_of(&d).to_string();
                    }
                }
            }
            for (_, found) in &files {
                if let Some((v, w)) = found {
                    dirs.insert((v.root, dir_of(&v.rel_path).to_string(), w.id));
                }
            }
            for w in &works {
                // The folder a work's own folder would be made in.
                for f in &w.files {
                    let parent = match f.role {
                        Role::Episode { .. } => flux_catalog::metafiles::show_dir(&f.rel_path)
                            .map(|d| dir_of(&d).to_string())
                            .unwrap_or_default(),
                        _ => dir_of(dir_of(&f.rel_path)).to_string(),
                    };
                    let name = work_name(w, &w.title, false);
                    dirs.insert((f.root, join(&parent, &name), w.id));
                    dirs.insert((f.root, join(dir_of(&f.rel_path), &name), w.id));
                }
            }
            for (root, dir, work) in dirs {
                let o = organize::other_works_under(c, root, &dir, work)?;
                others.insert((root, dir, work), o);
            }
            for w in &works {
                for f in &w.files {
                    if let Some(r) = organize::root(c, f.root)? {
                        roots.insert(r.id, r);
                    }
                }
            }
            for (_, found) in &files {
                if let Some((v, _)) = found {
                    if let Some(r) = organize::root(c, v.root)? {
                        roots.insert(r.id, r);
                    }
                }
            }
            Ok(Facts {
                works,
                files,
                others,
                roots,
            })
        })
        .await
        .map_err(cat_err)?;
    let hide = ctx.hide();
    let ids = input.ids_in_names;
    let (steps, skipped) = tokio::task::spawn_blocking(move || {
        let mut p = Planner {
            roots: facts.roots.clone(),
            hide,
            ids,
            steps: Vec::new(),
            skipped: Vec::new(),
            taken: BTreeSet::new(),
            made: BTreeSet::new(),
            renamed: Vec::new(),
            leaving: BTreeMap::new(),
        };
        for w in &facts.works {
            p.work(&facts, w);
        }
        for (m, found) in &facts.files {
            p.explicit(m, found.as_ref());
        }
        p.tidy();
        // Nothing but folders to make: nothing to do.
        if p.steps.iter().all(|s| s.field == organize::NEW_FOLDER) {
            p.steps.clear();
        }
        (p.steps, p.skipped)
    })
    .await
    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
    let roots_by_id: BTreeMap<Id, String> = {
        let ids: BTreeSet<Id> = steps.iter().map(|s| s.root).collect();
        ctx.st
            .catalog
            .read(move |c| {
                let mut m = BTreeMap::new();
                for id in ids {
                    if let Some(r) = organize::root(c, id)? {
                        m.insert(id, r.path);
                    }
                }
                Ok(m)
            })
            .await
            .map_err(cat_err)?
    };
    let out_steps: Vec<PlanStep> = steps
        .iter()
        .map(|s| PlanStep {
            step: match (s.field.as_str(), s.file) {
                (organize::NEW_FOLDER, _) => StepKind::NewFolder,
                (organize::RENAME_FOLDER, _) => StepKind::RenameFolder,
                (organize::REMOVE_FOLDER, _) => StepKind::RemoveFolder,
                (_, Some(_)) => StepKind::MoveVideo,
                (_, None) => StepKind::MoveCompanion,
            },
            file: s.file,
            item: (s.item != 0).then_some(s.item),
            root: roots_by_id.get(&s.root).cloned().unwrap_or_default(),
            from: s.from.clone(),
            to: s.to.clone(),
        })
        .collect();
    if steps.is_empty() {
        return Ok(PlanOutput {
            changeset: None,
            steps: Vec::new(),
            skipped,
        });
    }
    let committed = ctx
        .st
        .catalog
        .writer()
        .propose_plan(ctx.actor(), summary, steps)
        .await
        .map_err(cat_err)?;
    announce(&ctx.st, &committed);
    Ok(PlanOutput {
        changeset: committed.changeset,
        steps: out_steps,
        skipped,
    })
}

// ---- carrying a plan out ----------------------------------------------------------------------

/// Run one step forward (or, with `undo`, back). Returns why it wasn't
/// done.
fn run_step(root: &str, s: &organize::StepRow, undo: bool) -> Result<(), String> {
    let (from, to) = if undo { (&s.to, &s.from) } else { (&s.from, &s.to) };
    let check_rel_ok = |p: &str| -> Result<(), String> {
        if p.split('/').any(|x| x == ".." || x == "." || x.is_empty()) {
            return Err(format!("{p}: not a path inside the library folder"));
        }
        Ok(())
    };
    match (s.field.as_str(), undo) {
        // A folder is made: a new one, or (undoing) one the plan removed.
        (organize::NEW_FOLDER, false) | (organize::REMOVE_FOLDER, true) => {
            let p = to.as_deref().unwrap_or_default();
            check_rel_ok(p)?;
            if behind_link(root, dir_of(p)) {
                return Err("it's behind a link".into());
            }
            let a = abs(root, p);
            match std::fs::symlink_metadata(&a) {
                Ok(m) if m.is_dir() => Err("it's already there".into()),
                Ok(_) => Err("a file is there".into()),
                Err(_) => std::fs::create_dir(&a).map_err(|e| io_reason(&e)),
            }
        }
        // A folder goes: an emptied one, or (undoing) one the plan made.
        (organize::NEW_FOLDER, true) | (organize::REMOVE_FOLDER, false) => {
            let p = from.as_deref().unwrap_or_default();
            check_rel_ok(p)?;
            let a = abs(root, p);
            if !list(root, p).is_empty() {
                return Err("it isn't empty".into());
            }
            std::fs::remove_dir(&a).map_err(|e| io_reason(&e))
        }
        (organize::RENAME_FOLDER, _) | (organize::MOVE_FILE, _) => {
            let (Some(from), Some(to)) = (from.as_deref(), to.as_deref()) else {
                return Err("the step names no path".into());
            };
            check_rel_ok(from)?;
            check_rel_ok(to)?;
            if behind_link(root, from) || behind_link(root, dir_of(to)) {
                return Err("it's behind a link".into());
            }
            let (fa, ta) = (abs(root, from), abs(root, to));
            let meta = std::fs::symlink_metadata(&fa).map_err(|e| io_reason(&e))?;
            if meta.file_type().is_symlink() {
                return Err("it's a link".into());
            }
            let folder = s.field == organize::RENAME_FOLDER;
            if folder != meta.is_dir() {
                return Err(if folder {
                    "it isn't a folder"
                } else {
                    "it's a folder"
                }
                .into());
            }
            // A video must be the file the plan was made for.
            if !folder && !undo {
                if let Some(size) = s.size {
                    if meta.len() != size {
                        return Err("it changed since the plan was made".into());
                    }
                }
            }
            if !abs(root, dir_of(to)).is_dir() {
                return Err(format!("{} isn't there", dir_of(to)));
            }
            // Only the case of its name changes: the same entry on a
            // case-insensitive drive.
            if from.to_lowercase() == to.to_lowercase() {
                let tmp = abs(root, &format!("{from}.flux-{}", std::process::id()));
                rename_noreplace(&fa, &tmp).map_err(|e| io_reason(&e))?;
                return rename_noreplace(&tmp, &ta).map_err(|e| {
                    let _ = rename_noreplace(&tmp, &fa);
                    io_reason(&e)
                });
            }
            rename_noreplace(&fa, &ta).map_err(|e| io_reason(&e))
        }
        _ => Err("an unknown step".into()),
    }
}

/// Parse a video's new name as the scan would (siblings: what's beside it).
fn guess_for(root: &str, rel: &str, kind: LibraryKind) -> flux_core::NameGuess {
    let names: Vec<String> = list(root, dir_of(rel))
        .into_iter()
        .filter(|(_, d)| !d)
        .map(|(n, _)| n)
        .filter(|n| n != name_of(rel))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    flux_names::parse(Path::new(rel), &refs, kind)
}

/// Carry out (or with `undo` put back) the file plan `cs`, and record it.
pub async fn run(ctx: &Ctx, cs: Id, undo: bool) -> Result<flux_catalog::changes::Committed, OpError> {
    ctx.require_admin()?;
    let _one = MOVING.lock().await;
    let (state, rows, roots, kinds) = ctx
        .st
        .catalog
        .read(move |c| {
            let Some((state, rows)) = organize::steps(c, cs)? else {
                return Err(flux_catalog::CatalogError::NotFound(format!("changeset {cs}")));
            };
            let mut roots = BTreeMap::new();
            let mut kinds = BTreeMap::new();
            for r in &rows {
                if let Some(root) = organize::root(c, r.root)? {
                    roots.insert(r.root, root);
                }
                let k: Option<String> = c
                    .prepare_cached(
                        "SELECT l.kind FROM root r JOIN library l ON l.id = r.library_id WHERE r.id = ?1",
                    )?
                    .query_row([r.root], |x| x.get(0))
                    .ok();
                if let Some(k) = k.and_then(|k| LibraryKind::ALL.into_iter().find(|x| x.as_str() == k)) {
                    kinds.insert(r.root, k);
                }
            }
            Ok((state, rows, roots, kinds))
        })
        .await
        .map_err(cat_err)?;
    let want = if undo { "applied" } else { "proposed" };
    if state != want {
        return Err(err(
            ErrorCode::Conflict,
            format!("changeset {cs} is {state}, not {want}"),
        ));
    }
    // Before a video moves, the catalog must still have it where the plan
    // says (a rescan may have seen it move since): its path now, through
    // the folder renames of this plan that run before it.
    let renames: Vec<(i64, Id, String, String)> = rows
        .iter()
        .filter(|r| r.field == organize::RENAME_FOLDER)
        .filter_map(|r| Some((r.seq, r.root, r.from.clone()?, r.to.clone()?)))
        .collect();
    let paths: Vec<(i64, Id, Option<Id>, Option<String>)> = rows
        .iter()
        .map(|r| {
            (
                r.seq,
                r.root,
                r.file,
                if undo { r.to.clone() } else { r.from.clone() },
            )
        })
        .collect();
    let at_place: BTreeMap<i64, bool> = ctx
        .st
        .catalog
        .read(move |c| {
            let mut m = BTreeMap::new();
            for (seq, root, file, path) in paths {
                if let (Some(f), Some(p)) = (file, path) {
                    let now: Option<String> = c
                        .prepare_cached("SELECT rel_path FROM file WHERE id = ?1")?
                        .query_row([f], |r| r.get(0))
                        .ok();
                    // The catalog follows only once the whole plan has run:
                    // until then it has paths as they were before it.
                    let expected = now.map(|mut n| {
                        let before: Vec<&(i64, Id, String, String)> = if undo {
                            renames.iter().collect()
                        } else {
                            renames.iter().filter(|r| r.0 < seq).collect()
                        };
                        if !undo {
                            for (_, r, old, new) in before {
                                if *r != root {
                                    continue;
                                }
                                if n == *old {
                                    n = new.clone();
                                } else if let Some(rest) = n.strip_prefix(&format!("{old}/")) {
                                    n = format!("{new}/{rest}");
                                }
                            }
                        }
                        n
                    });
                    m.insert(seq, expected.as_deref() == Some(p.as_str()));
                }
            }
            Ok(m)
        })
        .await
        .map_err(cat_err)?;
    let done = tokio::task::spawn_blocking(move || {
        let mut order: Vec<&organize::StepRow> = rows
            .iter()
            .filter(|r| r.state == if undo { "applied" } else { "pending" })
            .collect();
        if undo {
            order.reverse();
        }
        let mut done = Vec::with_capacity(order.len());
        for s in order {
            let Some(root) = roots.get(&s.root) else {
                done.push(Done {
                    seq: s.seq,
                    failed: Some("its library folder is gone".into()),
                    guess: None,
                });
                continue;
            };
            if !root.available {
                done.push(Done {
                    seq: s.seq,
                    failed: Some("its library folder doesn't answer".into()),
                    guess: None,
                });
                continue;
            }
            if at_place.get(&s.seq) == Some(&false) {
                done.push(Done {
                    seq: s.seq,
                    failed: Some("it moved since the plan was made".into()),
                    guess: None,
                });
                continue;
            }
            let res = run_step(&root.path, s, undo);
            let guess = match (&res, s.file, undo) {
                (Ok(()), Some(_), false) => s.to.as_deref().map(|to| {
                    guess_for(
                        &root.path,
                        to,
                        kinds.get(&s.root).copied().unwrap_or(LibraryKind::Movies),
                    )
                }),
                _ => None,
            };
            if let Err(why) = &res {
                tracing::info!(changeset = cs, seq = s.seq, %why, "a plan step was not done");
            }
            done.push(Done {
                seq: s.seq,
                failed: res.err(),
                guess,
            });
        }
        done
    })
    .await
    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
    let committed = ctx
        .st
        .catalog
        .writer()
        .record_plan(cs, ctx.login(), done, undo)
        .await
        .map_err(cat_err)?;
    announce(&ctx.st, &committed);
    Ok(committed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_ones_every_system_accepts() {
        assert_eq!(clean_name("Mission: Impossible"), "Mission - Impossible");
        assert_eq!(clean_name("What? / Why*"), "What - Why-");
        assert_eq!(clean_name("  ...Dots and spaces. . "), "Dots and spaces");
        assert_eq!(clean_name("CON"), "CON_");
        assert_eq!(clean_name("com1.mkv"), "com1.mkv_");
        assert_eq!(clean_name("拾荒者统治"), "拾荒者统治");
        assert_eq!(clean_name("\"Quoted\" <x>"), "'Quoted' (x)");
        assert!(clean_name(&"長".repeat(100)).len() <= 180);
    }

    #[test]
    fn agent_paths_must_be_plain_and_relative() {
        assert_eq!(
            check_rel("Film (2010)/Film (2010).mkv").unwrap(),
            "Film (2010)/Film (2010).mkv"
        );
        assert!(check_rel("../x.mkv").is_err());
        assert!(check_rel("C:/x.mkv").is_err());
        assert!(check_rel("a\\b.mkv").is_err());
        assert!(check_rel("Mission: Impossible.mkv")
            .unwrap_err()
            .contains("Mission - Impossible"));
        assert!(check_rel("a//b.mkv").is_err());
    }

    #[test]
    fn the_deepest_common_folder() {
        assert_eq!(
            common_dir(["A/B".to_string(), "A/B/C".to_string()].into_iter()),
            "A/B"
        );
        assert_eq!(common_dir(["A".to_string(), "B".to_string()].into_iter()), "");
    }

    #[test]
    fn a_rename_never_replaces() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a.mkv"), d.path().join("b.mkv"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let e = rename_noreplace(&a, &b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&b).unwrap(), b"b");
        let c = d.path().join("c.mkv");
        rename_noreplace(&a, &c).unwrap();
        assert!(!a.exists() && c.exists());
        // Folders too.
        let (x, y) = (d.path().join("x"), d.path().join("y"));
        std::fs::create_dir(&x).unwrap();
        std::fs::create_dir(&y).unwrap();
        assert!(rename_noreplace(&x, &y).is_err());
        rename_noreplace(&x, &d.path().join("z")).unwrap();
    }
}
