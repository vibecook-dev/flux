//! Root-relative path helpers. Catalog paths are UTF-8, `/`-separated and
//! relative to the root, with no leading slash.

use std::ffi::OsStr;
use std::path::{Component, Path};

/// `dir/name`, or `name` at the root.
pub(crate) fn rel_join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        let mut s = String::with_capacity(dir.len() + 1 + name.len());
        s.push_str(dir);
        s.push('/');
        s.push_str(name);
        s
    }
}

/// `path` relative to `root`, `/`-joined. `None` when `path` is not under
/// `root` or a component is not valid UTF-8.
pub(crate) fn rel_of(root: &Path, path: &Path) -> Option<String> {
    let rest = path.strip_prefix(root).ok()?;
    let mut out = String::new();
    for c in rest.components() {
        match c {
            Component::Normal(s) => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(s.to_str()?);
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// The parent of a relative path (`""` for top-level entries).
pub(crate) fn parent_rel(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(p, _)| p)
}

/// `rel` is `dir` itself or inside it (`dir = ""` is the root).
pub(crate) fn is_under(rel: &str, dir: &str) -> bool {
    dir.is_empty()
        || (rel.len() > dir.len() && rel.starts_with(dir) && rel.as_bytes()[dir.len()] == b'/')
        || rel == dir
}

/// Some component starts with `.` (walk_scan's media profile never
/// delivers those).
pub(crate) fn has_hidden_component(rel: &str) -> bool {
    rel.split('/').any(|c| c.starts_with('.'))
}

/// The extension filter the walk applies on the read helper: video files
/// plus the sidecars flux-names uses as sibling evidence (one list, in
/// flux-core). The probe sniffs magic bytes, so this is only a pre-filter.
pub(crate) fn walk_extensions() -> impl Iterator<Item = &'static str> {
    flux_core::VIDEO_EXTENSIONS
        .iter()
        .chain(flux_core::SIDECAR_EXTENSIONS)
        .copied()
        // flux's own metadata files (`.flux.json`); `.nfo` is a sidecar.
        .chain(["json"])
}

/// A video file name (by extension).
pub(crate) fn is_video_name(name: &str) -> bool {
    flux_names::is_video_file(name)
}

/// Decode a directory entry name. Non-UTF-8 names can't be stored in the
/// catalog without risking aliasing (lossy conversion maps two names to
/// one), so the scanner skips them and counts them (integration.md §4.1 #4).
pub(crate) fn decode_name(name: &OsStr) -> Option<&str> {
    name.to_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_helpers() {
        assert_eq!(rel_join("", "a.mkv"), "a.mkv");
        assert_eq!(rel_join("TV/Show", "a.mkv"), "TV/Show/a.mkv");
        assert_eq!(
            rel_of(Path::new("/m"), Path::new("/m/a/b.mkv")).as_deref(),
            Some("a/b.mkv")
        );
        assert_eq!(rel_of(Path::new("/m"), Path::new("/m")).as_deref(), Some(""));
        assert_eq!(rel_of(Path::new("/m"), Path::new("/x/b.mkv")), None);
        assert_eq!(parent_rel("a/b/c.mkv"), "a/b");
        assert_eq!(parent_rel("c.mkv"), "");
        assert!(is_under("a/b/c.mkv", "a/b"));
        assert!(is_under("a/b", "a/b"));
        assert!(!is_under("a/bc/c.mkv", "a/b"));
        assert!(is_under("x", ""));
        assert!(has_hidden_component("a/.hidden/x.mkv"));
        assert!(!has_hidden_component("a/b.c/x.mkv"));
        assert!(is_video_name("x.MKV"));
        assert!(!is_video_name("x.srt"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_are_not_decoded() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(decode_name(OsStr::from_bytes(b"ok.mkv")), Some("ok.mkv"));
        assert_eq!(decode_name(OsStr::from_bytes(b"bad\xff.mkv")), None);
        let root = Path::new("/m");
        let p = root.join(OsStr::from_bytes(b"d\xfe/x.mkv"));
        assert_eq!(rel_of(root, &p), None);
    }
}
