//! Which file a path or an open handle names: what tells "the same file"
//! from "another file at the same path" (a file replaced or renamed over, a
//! symlink or junction re-pointed after the scan).
//!
//! On Unix that is `(st_dev, st_ino)`. On Windows std has no stable way to
//! read a file's identity, so this asks the handle for its volume serial
//! number and file id (`GetFileInformationByHandleEx(FileIdInfo)`, else
//! `GetFileInformationByHandle`): the same pair Windows itself uses to tell
//! files apart. The scan records it for files it reaches through a link
//! that leaves the library, and the server compares it before serving such a
//! file (review SEC-2).
//!
//! The one module of this crate that does I/O; it is here because the
//! scanner, the HLS engine and the server all need the same answer.

use std::fs::File;
use std::io;
use std::path::Path;

/// A file's identity, and whether it is still linked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    /// `st_dev` on Unix; the volume serial number on Windows.
    pub dev: u64,
    /// `st_ino` on Unix; the file id on Windows (a 128-bit id is folded to
    /// 64 bits; NTFS ids fit as they are).
    pub ino: u64,
    /// Hard links to the file; 0 once it is deleted while still open.
    pub links: u64,
}

impl FileId {
    /// The identity of the file `f` is open on.
    pub fn of_file(f: &File) -> io::Result<FileId> {
        imp::of_file(f)
    }

    /// The identity of the file `path` names, following links (a symlink's
    /// or junction's target). Works for directories too.
    pub fn of_path(path: &Path) -> io::Result<FileId> {
        imp::of_path(path)
    }

    /// Same file: device and file id (the link count is a state, not part
    /// of the identity).
    pub fn same_file(&self, other: &FileId) -> bool {
        (self.dev, self.ino) == (other.dev, other.ino)
    }
}

/// A file's modification time in nanoseconds since the Unix epoch (negative
/// before it), as the scan records it.
pub fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(meta.mtime_nsec())
    }
    #[cfg(not(unix))]
    {
        match meta.modified() {
            Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
                Err(e) => i64::try_from(e.duration().as_nanos()).map_or(i64::MIN, |n| -n),
            },
            Err(_) => 0,
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::FileId;
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    fn of_meta(m: &std::fs::Metadata) -> FileId {
        FileId {
            dev: m.dev(),
            ino: m.ino(),
            links: m.nlink(),
        }
    }

    pub(super) fn of_file(f: &File) -> io::Result<FileId> {
        Ok(of_meta(&f.metadata()?))
    }

    pub(super) fn of_path(path: &Path) -> io::Result<FileId> {
        Ok(of_meta(&std::fs::metadata(path)?))
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use super::FileId;
    use std::fs::File;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, FileStandardInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    };

    fn query<T: Default>(h: HANDLE, class: i32) -> Option<T> {
        let mut v = T::default();
        // SAFETY: `h` is a live handle owned by the caller's `File`; `v` is
        // a plain-data struct of the size we pass, which the call fills.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                h,
                class,
                (&mut v as *mut T).cast(),
                std::mem::size_of::<T>() as u32,
            )
        };
        (ok != 0).then_some(v)
    }

    pub(super) fn of_file(f: &File) -> io::Result<FileId> {
        let h = f.as_raw_handle() as HANDLE;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: as above; the call fills the struct we own.
        if unsafe { GetFileInformationByHandle(h, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // The 32-bit serial `vol` prints, whichever query names the file.
        let dev = u64::from(info.dwVolumeSerialNumber);
        // The 128-bit id where the file system has one (ReFS ids don't fit
        // in the 64-bit index; NTFS ids fold to the same 64-bit value).
        let ino = match query::<FILE_ID_INFO>(h, FileIdInfo) {
            Some(id) => {
                let b = id.FileId.Identifier;
                let lo = u64::from_le_bytes(b[..8].try_into().unwrap_or_default());
                let hi = u64::from_le_bytes(b[8..].try_into().unwrap_or_default());
                lo ^ hi
            }
            None => (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        };
        // A file deleted while open is "delete pending" (or, with POSIX
        // delete semantics, has no link left): either way, gone.
        let links = match query::<FILE_STANDARD_INFO>(h, FileStandardInfo) {
            Some(s) if s.DeletePending => 0,
            Some(s) => u64::from(s.NumberOfLinks),
            None => u64::from(info.nNumberOfLinks),
        };
        Ok(FileId { dev, ino, links })
    }

    pub(super) fn of_path(path: &Path) -> io::Result<FileId> {
        // Attributes only (no data access; the same right a `File::open`
        // handle has, so both answer the same queries), sharing everything,
        // so asking never gets in the way of another program; backup
        // semantics open directories too. Links are followed (no
        // OPEN_REPARSE_POINT).
        let f = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        of_file(&f)
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::FileId;
    use std::fs::File;
    use std::io;
    use std::path::Path;

    pub(super) fn of_file(_f: &File) -> io::Result<FileId> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(super) fn of_path(_path: &Path) -> io::Result<FileId> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("flux-file-id-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_path_and_its_open_file_agree_and_two_files_differ() {
        let d = scratch("agree");
        let (a, b) = (d.join("a.mkv"), d.join("b.mkv"));
        std::fs::write(&a, b"aaaa").unwrap();
        std::fs::write(&b, b"aaaa").unwrap();
        let fa = File::open(&a).unwrap();
        let (ia, ib) = (FileId::of_path(&a).unwrap(), FileId::of_path(&b).unwrap());
        assert!(ia.same_file(&FileId::of_file(&fa).unwrap()));
        assert!(!ia.same_file(&ib), "{ia:?} vs {ib:?}");
        assert_eq!(ia.dev, ib.dev, "one volume");
        assert_eq!(ia.links, 1);
        // Directories have an identity too (device keys use the root's).
        assert_eq!(FileId::of_path(&d).unwrap().dev, ia.dev);
        // A hard link is the same file, and counts.
        let h = d.join("hard.mkv");
        std::fs::hard_link(&a, &h).unwrap();
        let ih = FileId::of_path(&h).unwrap();
        assert!(ih.same_file(&ia));
        assert_eq!(ih.links, 2);
        drop(fa);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_replaced_at_its_path_is_another_file() {
        let d = scratch("replaced");
        let p = d.join("film.mkv");
        std::fs::write(&p, b"old").unwrap();
        let f = File::open(&p).unwrap();
        let before = FileId::of_file(&f).unwrap();
        let tmp = d.join("incoming.mkv");
        std::fs::write(&tmp, b"new").unwrap();
        std::fs::rename(&tmp, &p).unwrap();
        assert!(!FileId::of_path(&p).unwrap().same_file(&before));
        assert!(
            FileId::of_file(&f).unwrap().same_file(&before),
            "the handle keeps its file"
        );
        drop(f);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_deleted_while_open_has_no_links() {
        let d = scratch("deleted");
        let p = d.join("film.mkv");
        std::fs::write(&p, b"x").unwrap();
        let f = File::open(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(FileId::of_file(&f).unwrap().links, 0);
        assert!(FileId::of_path(&p).is_err());
        drop(f);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn mtime_matches_system_time() {
        let d = scratch("mtime");
        let p = d.join("f");
        std::fs::write(&p, b"x").unwrap();
        let m = std::fs::metadata(&p).unwrap();
        let st = m
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        assert_eq!(mtime_ns(&m), st);
        let _ = std::fs::remove_dir_all(&d);
    }
}
