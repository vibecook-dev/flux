//! Container identification from magic bytes. Extensions are only a hint
//! for how much to read first; they never decide the format.

use std::path::Path;

use flux_core::Container;

/// Bytes the sniffer wants (enough for 5 M2TS packets).
pub(crate) const SNIFF_LEN: u64 = 2048;

/// What the caller believes the container is (usually from the file
/// extension). It only sizes the first read: TS-like hints read 256 KiB up
/// front because MPEG-TS needs the PAT/PMT and first PES of every stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContainerHint(pub Option<Container>);

impl ContainerHint {
    /// No hint.
    pub const NONE: ContainerHint = ContainerHint(None);

    /// Hint from a file extension (case-insensitive, without the dot).
    pub fn from_extension(ext: &str) -> Self {
        let c = match ext.to_ascii_lowercase().as_str() {
            "mkv" | "mka" | "mks" | "mk3d" => Container::Matroska,
            "webm" => Container::WebM,
            "mp4" | "m4v" | "m4a" | "3gp" | "f4v" => Container::Mp4,
            "mov" | "qt" => Container::Mov,
            "ts" | "mts" | "tp" | "trp" => Container::MpegTs,
            "m2ts" | "m2t" => Container::M2ts,
            "avi" | "divx" => Container::Avi,
            _ => return ContainerHint::NONE,
        };
        ContainerHint(Some(c))
    }

    /// Hint from a path's extension.
    pub fn from_path(path: &Path) -> Self {
        path.extension()
            .and_then(|e| e.to_str())
            .map_or(ContainerHint::NONE, Self::from_extension)
    }
}

/// Identify the container from the first bytes of the file.
pub fn sniff(head: &[u8]) -> Option<Container> {
    if head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        // DocType decides Matroska vs WebM later.
        return Some(Container::Matroska);
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && matches!(&head[8..12], b"AVI " | b"AVIX") {
        return Some(Container::Avi);
    }
    if is_iso_bmff(head) {
        return Some(Container::Mp4);
    }
    ts_layout(head).map(|(c, _)| c)
}

fn is_iso_bmff(head: &[u8]) -> bool {
    let Some(ty) = head.get(4..8) else { return false };
    let size = head
        .get(..4)
        .map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let known = matches!(
        ty,
        b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"pnot" | b"styp" | b"sidx" | b"moof"
    );
    known && (size == 0 || size == 1 || size >= 8)
}

/// Detect MPEG-TS (188-byte packets) or BDAV M2TS (192 = 4-byte timecode +
/// 188). Returns the container and the offset of the first sync byte.
pub(crate) fn ts_layout(b: &[u8]) -> Option<(Container, usize)> {
    if let Some(off) = sync_offset(b, 188, 0) {
        return Some((Container::MpegTs, off));
    }
    sync_offset(b, 192, 4).map(|off| (Container::M2ts, off))
}

/// Offset of the first 0x47 sync byte for a given packet `stride` (with
/// `lead` bytes of per-packet prefix), confirmed by up to 5 packets.
pub(crate) fn sync_offset(b: &[u8], stride: usize, lead: usize) -> Option<usize> {
    for start in 0..stride {
        let first = start + lead;
        let packets = b.len().saturating_sub(first) / stride;
        if packets < 3 {
            break;
        }
        let need = packets.min(5);
        if (0..need).all(|k| b.get(first + k * stride) == Some(&0x47)) {
            return Some(first);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic() {
        assert_eq!(sniff(&[0x1a, 0x45, 0xdf, 0xa3, 0x01]), Some(Container::Matroska));
        assert_eq!(sniff(b"RIFF\0\0\0\0AVI LIST"), Some(Container::Avi));
        assert_eq!(sniff(b"\0\0\0\x20ftypisom"), Some(Container::Mp4));
        assert_eq!(sniff(b"garbage!garbage!"), None);
        assert_eq!(sniff(&[]), None);
        let mut ts = vec![0u8; 188 * 5];
        for k in 0..5 {
            ts[k * 188] = 0x47;
        }
        assert_eq!(sniff(&ts), Some(Container::MpegTs));
        let mut m2ts = vec![0u8; 192 * 5];
        for k in 0..5 {
            m2ts[k * 192 + 4] = 0x47;
        }
        assert_eq!(sniff(&m2ts), Some(Container::M2ts));
    }

    #[test]
    fn hint() {
        assert_eq!(ContainerHint::from_extension("M2TS").0, Some(Container::M2ts));
        assert_eq!(ContainerHint::from_extension("f4v").0, Some(Container::Mp4));
        assert_eq!(
            ContainerHint::from_path(Path::new("a/b.txt")),
            ContainerHint::NONE
        );
    }
}
