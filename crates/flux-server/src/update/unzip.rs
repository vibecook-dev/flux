//! Unpacks the Windows release package, a `.zip` that
//! scripts/package/zip.py writes: stored folders and deflated files, sizes in
//! the central directory, no encryption and no ZIP64 (a package is ~25 MB).
//! Anything else is refused, as is any entry that would land outside the
//! folder it is unpacked into.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, ensure, Context};

const END: u32 = 0x0605_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const LOCAL: u32 = 0x0403_4b50;
/// No file in a package comes near this; a larger size is a broken archive.
const MAX_FILE: u64 = 1 << 30;

/// One entry, from the central directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    local: u64,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

fn u16_at(b: &[u8], at: usize) -> anyhow::Result<u16> {
    let s = b.get(at..at + 2).context("zip: truncated")?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], at: usize) -> anyhow::Result<u32> {
    let s = b.get(at..at + 4).context("zip: truncated")?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// The archive's entries.
pub fn entries(zip: &[u8]) -> anyhow::Result<Vec<Entry>> {
    // The end record is the last 22 bytes, unless a comment follows it.
    let search_from = zip.len().saturating_sub(22 + 0xFFFF);
    let end = (search_from..=zip.len().saturating_sub(22))
        .rev()
        .find(|&i| u32_at(zip, i).ok() == Some(END))
        .context("zip: no end of central directory")?;
    let count = u16_at(zip, end + 10)?;
    let cd_size = u32_at(zip, end + 12)?;
    let cd_offset = u32_at(zip, end + 16)?;
    ensure!(
        count != 0xFFFF && cd_size != u32::MAX && cd_offset != u32::MAX,
        "zip: ZIP64 archives aren't flux packages"
    );
    let mut at = cd_offset as usize;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        ensure!(u32_at(zip, at)? == CENTRAL, "zip: broken central directory");
        let flags = u16_at(zip, at + 8)?;
        ensure!(flags & 1 == 0, "zip: encrypted entries aren't flux packages");
        let method = u16_at(zip, at + 10)?;
        let crc = u32_at(zip, at + 16)?;
        let compressed = u32_at(zip, at + 20)?;
        let size = u32_at(zip, at + 24)?;
        let name_len = u16_at(zip, at + 28)? as usize;
        let extra_len = u16_at(zip, at + 30)? as usize;
        let comment_len = u16_at(zip, at + 32)? as usize;
        let local = u32_at(zip, at + 42)?;
        ensure!(
            compressed != u32::MAX && size != u32::MAX && local != u32::MAX,
            "zip: ZIP64 entries aren't flux packages"
        );
        let name = zip.get(at + 46..at + 46 + name_len).context("zip: truncated")?;
        let name = String::from_utf8(name.to_vec()).context("zip: an entry's name isn't UTF-8")?;
        out.push(Entry {
            name,
            method,
            crc,
            compressed: compressed.into(),
            size: size.into(),
            local: local.into(),
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

/// An entry's bytes, checked against its size and CRC.
pub fn read(zip: &[u8], e: &Entry) -> anyhow::Result<Vec<u8>> {
    ensure!(e.size <= MAX_FILE, "zip: {} is too large", e.name);
    let at = e.local as usize;
    ensure!(u32_at(zip, at)? == LOCAL, "zip: {}: no local header", e.name);
    let start = at + 30 + u16_at(zip, at + 26)? as usize + u16_at(zip, at + 28)? as usize;
    let data = zip
        .get(start..start + e.compressed as usize)
        .with_context(|| format!("zip: {} is truncated", e.name))?;
    let bytes = match e.method {
        0 => data.to_vec(),
        8 => {
            let mut out = Vec::with_capacity(e.size as usize);
            // One byte more than promised shows a size that lies.
            flate2::read::DeflateDecoder::new(data)
                .take(e.size + 1)
                .read_to_end(&mut out)
                .with_context(|| format!("zip: {} doesn't inflate", e.name))?;
            out
        }
        m => bail!(
            "zip: {}: compression method {m} isn't used by flux packages",
            e.name
        ),
    };
    ensure!(bytes.len() as u64 == e.size, "zip: {} has the wrong size", e.name);
    ensure!(crc32fast::hash(&bytes) == e.crc, "zip: {} fails its CRC", e.name);
    Ok(bytes)
}

/// Where an entry goes under `into`, or an error for a name that would
/// leave it (absolute, `..`, a drive, a backslash).
pub fn target(into: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let rel = name.trim_end_matches('/');
    let bad = rel.is_empty() || rel.contains('\\') || rel.contains(':') || rel.starts_with('/');
    ensure!(!bad, "zip: entry {name:?} would land outside the folder");
    let mut out = into.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(part) => out.push(part),
            _ => bail!("zip: entry {name:?} would land outside the folder"),
        }
    }
    Ok(out)
}

/// Unpacks every entry under `into`.
pub fn unzip(zip: &[u8], into: &Path) -> anyhow::Result<()> {
    for e in entries(zip)? {
        let to = target(into, &e.name)?;
        if e.is_dir() {
            std::fs::create_dir_all(&to)?;
            continue;
        }
        let bytes = read(zip, &e)?;
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&to, bytes).with_context(|| format!("writing {}", to.display()))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::*;

    /// A zip like zip.py's: (name, bytes, deflated); names ending in `/` are folders.
    pub(crate) fn zip_of(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data, deflate) in files {
            let (method, body) = if *deflate {
                let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
                enc.write_all(data).unwrap();
                (8u16, enc.finish().unwrap())
            } else {
                (0u16, data.to_vec())
            };
            let crc = crc32fast::hash(data);
            let offset = out.len() as u32;
            out.extend(LOCAL.to_le_bytes());
            out.extend([20, 0, 0, 0]);
            out.extend(method.to_le_bytes());
            out.extend([0, 0, 0x21, 0]);
            out.extend(crc.to_le_bytes());
            out.extend((body.len() as u32).to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend([0, 0]);
            out.extend(name.as_bytes());
            out.extend(&body);
            central.extend(CENTRAL.to_le_bytes());
            central.extend([20, 3, 20, 0, 0, 0]);
            central.extend(method.to_le_bytes());
            central.extend([0, 0, 0x21, 0]);
            central.extend(crc.to_le_bytes());
            central.extend((body.len() as u32).to_le_bytes());
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend(offset.to_le_bytes());
            central.extend(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend(&central);
        out.extend(END.to_le_bytes());
        out.extend([0, 0, 0, 0]);
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((central.len() as u32).to_le_bytes());
        out.extend(cd_offset.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    #[test]
    fn unpacks_a_package_like_zip_py_writes() {
        let exe = vec![0x4du8; 70_000];
        let zip = zip_of(&[
            ("flux-1.0.0-x86_64-pc-windows-msvc/", b"", false),
            ("flux-1.0.0-x86_64-pc-windows-msvc/flux-server.exe", &exe, true),
            ("flux-1.0.0-x86_64-pc-windows-msvc/web/", b"", false),
            (
                "flux-1.0.0-x86_64-pc-windows-msvc/web/index.html",
                b"<!doctype html>",
                true,
            ),
            (
                "flux-1.0.0-x86_64-pc-windows-msvc/install-kind",
                b"archive\n",
                false,
            ),
        ]);
        let d = tempfile::tempdir().unwrap();
        unzip(&zip, d.path()).unwrap();
        let top = d.path().join("flux-1.0.0-x86_64-pc-windows-msvc");
        assert_eq!(std::fs::read(top.join("flux-server.exe")).unwrap(), exe);
        assert_eq!(
            std::fs::read_to_string(top.join("web/index.html")).unwrap(),
            "<!doctype html>"
        );
        assert_eq!(
            std::fs::read_to_string(top.join("install-kind")).unwrap(),
            "archive\n"
        );
    }

    #[test]
    fn refuses_entries_that_would_land_outside() {
        let d = tempfile::tempdir().unwrap();
        for name in ["../evil.exe", "/etc/x", "a/../../x", "C:/x", "a\\..\\x", "/"] {
            let zip = zip_of(&[(name, b"x", false)]);
            assert!(unzip(&zip, d.path()).is_err(), "{name}");
        }
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0, "nothing written");
    }

    #[test]
    fn refuses_a_damaged_archive() {
        let mut zip = zip_of(&[("a/b.txt", b"hello hello hello", true)]);
        // Flip a byte of the compressed data: the CRC (or inflating) catches it.
        let at = 30 + "a/b.txt".len() + 2;
        zip[at] ^= 0xFF;
        let d = tempfile::tempdir().unwrap();
        assert!(unzip(&zip, d.path()).is_err());
        assert!(entries(b"not a zip at all, just text").is_err());
        assert!(entries(&zip[..zip.len() - 10]).is_err(), "truncated");
    }
}
