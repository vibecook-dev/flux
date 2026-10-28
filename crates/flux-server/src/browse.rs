//! The folder browser the setup screen adds libraries with (Owners and
//! Admins, `GET /api/v1/admin/browse`): the folders on this server, how many
//! videos each holds, and what a folder's videos look like (films, shows,
//! home videos), so a person picks a folder rather than typing its path.
//!
//! Names only: no file is opened, and only links are followed with a `stat`.
//! Counting stops at a time budget, checked at every entry, because a
//! sleeping NAS or a folder of 100,000 files answers slowly, and a count it
//! didn't finish is `None` (the app shows nothing rather than a wrong
//! number). A folder that doesn't answer at all (a dead network mount) can
//! still hold a thread, so the API runs a few of these at a time, each under
//! a timeout (`api::browse`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use flux_core::api::{BrowseFolder, FolderSummary};
use flux_core::names::{GuessKind, LibraryKind};

/// How long counting the videos of a listing's folders may take, in all.
const COUNT_BUDGET: Duration = Duration::from_millis(700);
/// How long looking at the chosen folder's videos may take.
const SUMMARY_BUDGET: Duration = Duration::from_millis(1500);
/// How long listing a folder's folders may take (the rest are left out).
const LIST_BUDGET: Duration = Duration::from_secs(3);
/// Videos counted per folder, at most (more shows as "1,000+").
pub const COUNT_CAP: u32 = 1000;
/// Folders listed, at most.
const LIST_CAP: usize = 2000;
/// Videos whose names the summary reads, at most.
const SUMMARY_CAP: u32 = 600;
/// Folder levels counted below a listed folder (`Films/Heat (1995)/Heat.mkv`).
const COUNT_DEPTH: usize = 2;
/// Levels the summary looks into (`Shows/Severance/Season 2/S02E10.mkv`).
const SUMMARY_DEPTH: usize = 4;

/// Folders no library wants, wherever they are: recycle bins, NAS
/// thumbnails and snapshots.
const SKIP: &[&str] = &[
    "$RECYCLE.BIN",
    "System Volume Information",
    "@eaDir",
    "#recycle",
    "#snapshot",
    "lost+found",
];

/// The system's own folders, left out at the top of a disk or a home folder
/// only (a `Library` folder on a media drive is someone's films).
#[cfg(target_os = "macos")]
const SYSTEM: &[&str] = &[
    "Library",
    "System",
    "Applications",
    "Volumes",
    "private",
    "cores",
    "opt",
    "usr",
    "bin",
    "sbin",
    "etc",
    "var",
    "tmp",
    "dev",
];
#[cfg(windows)]
const SYSTEM: &[&str] = &[
    "Windows",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "Recovery",
    "PerfLogs",
    "AppData",
];
#[cfg(not(any(target_os = "macos", windows)))]
const SYSTEM: &[&str] = &[
    "bin", "boot", "dev", "etc", "lib", "lib32", "lib64", "libx32", "proc", "run", "sbin", "snap", "sys",
    "tmp", "usr", "var",
];

fn hidden(name: &str) -> bool {
    name.starts_with('.') || SKIP.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// Whether `dir` is the top of a disk or the home folder, where the
/// system's own folders are left out.
fn system_level(dir: &Path) -> bool {
    dir.parent().is_none() || home_dir().is_some_and(|h| h == dir)
}

fn is_dir(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_dir())
}

/// Whether a directory entry is a folder: from the listing itself, and only
/// a link costs a `stat` (of its target).
fn entry_is_dir(e: &std::fs::DirEntry) -> bool {
    match e.file_type() {
        Ok(t) if t.is_symlink() => is_dir(&e.path()),
        Ok(t) => t.is_dir(),
        Err(_) => false,
    }
}

/// Paths the browser won't read: Windows device paths (`\\.\`, `\\?\`),
/// which aren't folders of files. Shares (`\\nas\media`) are fine.
pub fn refused(path: &str) -> bool {
    let p = path.replace('/', "\\");
    p.starts_with("\\\\.\\") || p.starts_with("\\\\?\\")
}

/// Where browsing starts: home, this machine's volumes and drives, the
/// usual mount points, and the whole machine.
pub fn starting_places() -> Vec<BrowseFolder> {
    let mut out: Vec<BrowseFolder> = Vec::new();
    let mut push = |name: String, path: PathBuf| {
        let p = path.to_string_lossy().into_owned();
        if !out.iter().any(|f| f.path == p) {
            out.push(BrowseFolder {
                name,
                path: p,
                videos: None,
            });
        }
    };
    if let Some(home) = home_dir().filter(|h| is_dir(h)) {
        push("Home".into(), home);
    }
    #[cfg(windows)]
    for drive in windows_drives() {
        push(drive.clone(), PathBuf::from(drive));
    }
    #[cfg(not(windows))]
    {
        // macOS: each mounted volume but the startup disk (a link to /). The
        // listing says which are folders, without touching a sleeping share.
        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            let mut vols: Vec<PathBuf> = rd
                .flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect();
            vols.sort();
            for v in vols {
                let name = v
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                push(name, v);
            }
        }
        // Linux (and the Docker image's /media): mount points with something in them.
        for place in ["/media", "/mnt", "/srv", "/data"] {
            let p = Path::new(place);
            let has_any = std::fs::read_dir(p).is_ok_and(|mut rd| rd.any(|e| e.is_ok()));
            if is_dir(p) && has_any {
                push(place.into(), p.to_path_buf());
            }
        }
        push("This computer".into(), PathBuf::from("/"));
    }
    out
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|h| !h.is_empty()).map(PathBuf::from)
}

#[cfg(windows)]
fn windows_drives() -> Vec<String> {
    // GetLogicalDrives: a bit per drive letter, without touching any (a
    // disconnected network drive would make that slow).
    #[allow(unsafe_code)]
    // SAFETY: no arguments; returns a bitmask.
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .collect()
}

/// The folders in `dir` (hidden and system ones left out), sorted, each with
/// its video count while the budget lasts.
pub fn list(dir: &Path) -> std::io::Result<Vec<BrowseFolder>> {
    let deadline = Instant::now() + LIST_BUDGET;
    let top = system_level(dir);
    let mut folders: Vec<(String, PathBuf)> = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        if folders.len() >= LIST_CAP || Instant::now() > deadline {
            break;
        }
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        if hidden(&name) || (top && SYSTEM.iter().any(|s| s.eq_ignore_ascii_case(&name))) || !entry_is_dir(&e)
        {
            continue;
        }
        folders.push((name, e.path()));
    }
    folders.sort_by_key(|(n, _)| n.to_lowercase());
    let start = Instant::now();
    Ok(folders
        .into_iter()
        .map(|(name, path)| {
            let videos = (start.elapsed() < COUNT_BUDGET)
                .then(|| count_videos(&path, COUNT_DEPTH, start + COUNT_BUDGET))
                .flatten();
            BrowseFolder {
                name,
                path: path.to_string_lossy().into_owned(),
                videos,
            }
        })
        .collect())
}

/// Video files in `dir` and `depth` levels below, up to [`COUNT_CAP`];
/// `None` if `deadline` passed first.
fn count_videos(dir: &Path, depth: usize, deadline: Instant) -> Option<u32> {
    let mut n = 0u32;
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, level)) = stack.pop() {
        if Instant::now() > deadline {
            return None;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if Instant::now() > deadline {
                return None;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if hidden(&name) {
                continue;
            }
            let is_folder = e.file_type().is_ok_and(|t| t.is_dir());
            if is_folder {
                if level < depth {
                    stack.push((e.path(), level + 1));
                }
            } else if flux_names::is_video_file(&name) {
                n += 1;
                if n >= COUNT_CAP {
                    return Some(COUNT_CAP);
                }
            }
        }
    }
    Some(n)
}

/// What the videos in `dir` look like: how many, how many read as films or
/// as episodes, and the library kind that fits.
pub fn summarize(dir: &Path) -> FolderSummary {
    let deadline = Instant::now() + SUMMARY_BUDGET;
    let (mut videos, mut films, mut episodes, mut more) = (0u32, 0u32, 0u32, false);
    let mut stack = vec![(PathBuf::new(), 0usize)];
    'walk: while let Some((rel, level)) = stack.pop() {
        if Instant::now() > deadline {
            more = true;
            break;
        }
        let Ok(rd) = std::fs::read_dir(dir.join(&rel)) else {
            continue;
        };
        for e in rd.flatten() {
            if Instant::now() > deadline {
                more = true;
                break 'walk;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if hidden(&name) {
                continue;
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                if level < SUMMARY_DEPTH {
                    stack.push((rel.join(&name), level + 1));
                }
                continue;
            }
            if !flux_names::is_video_file(&name) {
                continue;
            }
            videos += 1;
            let guess = flux_names::parse(&rel.join(&name), &[], LibraryKind::Mixed);
            match guess.kind {
                GuessKind::Episode => episodes += 1,
                // A film needs a year or a confident title: "IMG_1234" isn't one.
                GuessKind::Movie if guess.year.is_some() || guess.confidence >= 0.6 => films += 1,
                _ => {}
            }
            if videos >= SUMMARY_CAP {
                more = true;
                break 'walk;
            }
        }
    }
    FolderSummary {
        videos,
        more,
        films,
        episodes,
        kind: kind_for(videos, films, episodes),
        taken_by: None,
    }
}

/// The library kind for videos that read as `films` films and `episodes`
/// episodes: mostly one or the other, both, or (names that read as
/// neither, like a camera's) home videos.
pub fn kind_for(videos: u32, films: u32, episodes: u32) -> Option<LibraryKind> {
    let known = films + episodes;
    if videos == 0 {
        None
    } else if known * 2 < videos {
        Some(LibraryKind::Videos)
    } else if episodes * 5 >= known * 4 {
        Some(LibraryKind::Shows)
    } else if films * 5 >= known * 4 {
        Some(LibraryKind::Movies)
    } else {
        Some(LibraryKind::Mixed)
    }
}

/// The folder above `dir` (none above a drive or `/`).
pub fn parent_of(dir: &Path) -> Option<String> {
    dir.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn lists_folders_with_their_videos_and_leaves_out_the_rest() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "Films/Heat (1995)/Heat (1995).mkv");
        touch(d.path(), "Films/Alien (1979).mp4");
        touch(d.path(), "Films/poster.jpg");
        touch(d.path(), "TV/Severance/Season 1/Severance S01E01.mkv");
        touch(d.path(), "TV/Severance/Season 1/Extras/Deep/Making of.mkv");
        touch(d.path(), ".hidden/x.mkv");
        touch(d.path(), "@eaDir/thumbs.mkv");
        touch(d.path(), "notes.txt");
        let got = list(d.path()).unwrap();
        let names: Vec<(&str, Option<u32>)> = got.iter().map(|f| (f.name.as_str(), f.videos)).collect();
        // Show/Season/episode is counted from the top; deeper than that isn't.
        assert_eq!(names, [("Films", Some(2)), ("TV", Some(1))]);
        assert!(got[0].path.ends_with("Films"));
    }

    #[test]
    fn tells_films_from_shows_from_home_videos() {
        let films = tempfile::tempdir().unwrap();
        for f in [
            "Heat (1995)/Heat (1995).mkv",
            "Alien (1979).mp4",
            "Blade Runner 1982.mkv",
            "Arrival.2016.1080p.mkv",
        ] {
            touch(films.path(), f);
        }
        let s = summarize(films.path());
        assert_eq!(
            (s.videos, s.films, s.episodes, s.kind),
            (4, 4, 0, Some(LibraryKind::Movies)),
            "{s:?}"
        );

        let shows = tempfile::tempdir().unwrap();
        for f in [
            "Severance/Season 1/Severance S01E01.mkv",
            "Severance/Season 1/Severance S01E02.mkv",
            "The Bear/The.Bear.S02E03.mkv",
        ] {
            touch(shows.path(), f);
        }
        assert_eq!(summarize(shows.path()).kind, Some(LibraryKind::Shows));

        let both = tempfile::tempdir().unwrap();
        for f in [
            "Heat (1995).mkv",
            "Alien (1979).mkv",
            "Severance S01E01.mkv",
            "Severance S01E02.mkv",
        ] {
            touch(both.path(), f);
        }
        assert_eq!(summarize(both.path()).kind, Some(LibraryKind::Mixed));

        let camera = tempfile::tempdir().unwrap();
        for f in [
            "IMG_1234.MOV",
            "IMG_1235.MOV",
            "VID_20240704_101500.mp4",
            "DSC0001.MTS",
        ] {
            touch(camera.path(), f);
        }
        assert_eq!(
            summarize(camera.path()).kind,
            Some(LibraryKind::Videos),
            "{:?}",
            summarize(camera.path())
        );

        let empty = tempfile::tempdir().unwrap();
        touch(empty.path(), "readme.txt");
        assert_eq!(summarize(empty.path()).kind, None);
    }

    /// A media drive's "Library" folder was left out, as if it were macOS's.
    #[test]
    fn system_folders_are_left_out_at_the_top_only() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "Media/Library/Heat (1995).mkv");
        touch(d.path(), "Media/System/x.mkv");
        let got = list(&d.path().join("Media")).unwrap();
        let names: Vec<&str> = got.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Library", "System"]);
        let root = list(Path::new(if cfg!(windows) { "C:\\" } else { "/" })).unwrap();
        let system = if cfg!(windows) {
            "Windows"
        } else if cfg!(target_os = "macos") {
            "System"
        } else {
            "proc"
        };
        assert!(
            root.iter().all(|f| !f.name.eq_ignore_ascii_case(system)),
            "{root:?}"
        );
    }

    #[test]
    fn device_paths_are_refused_and_shares_are_not() {
        assert!(refused("\\\\.\\pipe\\x"));
        assert!(refused("\\\\?\\GLOBALROOT\\Device"));
        assert!(refused("//./c:/"));
        assert!(!refused("\\\\nas\\media"));
        assert!(!refused("/Volumes/Media"));
        assert!(!refused("C:\\Users"));
    }

    #[test]
    fn kinds_by_count() {
        assert_eq!(kind_for(0, 0, 0), None);
        assert_eq!(kind_for(10, 10, 0), Some(LibraryKind::Movies));
        assert_eq!(kind_for(10, 1, 9), Some(LibraryKind::Shows));
        assert_eq!(kind_for(10, 5, 5), Some(LibraryKind::Mixed));
        assert_eq!(kind_for(10, 2, 2), Some(LibraryKind::Videos));
    }

    #[test]
    fn starts_at_home_and_the_whole_machine() {
        let places = starting_places();
        assert!(!places.is_empty());
        #[cfg(not(windows))]
        assert_eq!(places.last().map(|p| p.path.as_str()), Some("/"));
    }
}
