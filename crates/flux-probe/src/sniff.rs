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
            "mp4" | "m4v" | "m4a" | "3gp" => Container::Mp4,
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

/// Bytes [`not_media`] wants from the start of the file. The probe has read
/// at least this much already (its first read is 64 KiB, 256 KiB for TS
/// names), so a caller with a page cache pays no extra I/O.
pub const NOT_MEDIA_SNIFF_LEN: u64 = 64 * 1024;

/// Why a file that no prober recognised (`ProbeError::UnknownFormat`) is
/// not media at all, judged from its first bytes (up to
/// [`NOT_MEDIA_SNIFF_LEN`]), or `None` when it may still be: ffprobe reads
/// many containers flux-probe doesn't (ASF, FLV, RealMedia, MPEG-PS…).
///
/// - **Text**: valid UTF-8 with no control bytes but tab, line breaks and
///   form feed. Source code (a TypeScript `.ts`), subtitles and playlists
///   are text; every media container starts with binary structure.
/// - **A TS-like name** (`.ts`, `.mts`, `.m2ts`, `.tp`…) **without MPEG-TS
///   packets**: no 0x47 sync byte every 188, 192 or 204 bytes, and no other
///   container's signature.
pub fn not_media(head: &[u8], hint: ContainerHint) -> Option<&'static str> {
    if head.is_empty() || sniff(head).is_some() || other_container(head) {
        return None;
    }
    if is_text(head) {
        return Some("a text file, not a video");
    }
    if matches!(hint.0, Some(Container::MpegTs | Container::M2ts)) && !has_ts_packets(head) {
        return Some("no MPEG-TS packets (no 0x47 sync byte every 188, 192 or 204 bytes)");
    }
    None
}

/// Signatures of containers ffprobe reads and flux-probe doesn't.
fn other_container(head: &[u8]) -> bool {
    const SIGNATURES: &[&[u8]] = &[
        &[0x30, 0x26, 0xb2, 0x75, 0x8e, 0x66, 0xcf, 0x11], // ASF (WMV)
        b"FLV\x01",                                        // Flash video
        b".RMF",                                           // RealMedia
        &[0x00, 0x00, 0x01, 0xba],                         // MPEG program stream
        &[0x00, 0x00, 0x01, 0xb3],                         // MPEG video elementary stream
        b"OggS",
        b"RIFF",
        b"ID3",
    ];
    SIGNATURES.iter().any(|s| head.starts_with(s))
}

/// Valid UTF-8 (a sequence cut off at the end of `head` is fine) without
/// control bytes other than tab, line feed, carriage return and form feed.
fn is_text(head: &[u8]) -> bool {
    let valid = match std::str::from_utf8(head) {
        Ok(_) => head.len(),
        // Cut off mid-character at the end of the sample: still text.
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => return false,
    };
    head[..valid]
        .iter()
        .all(|&b| b >= 0x20 || matches!(b, b'\t' | b'\n' | b'\r' | 0x0c))
}

/// Three 0x47 sync bytes one packet apart (188, 192 or 204 bytes: MPEG-TS,
/// BDAV M2TS, TS with Reed-Solomon parity) starting anywhere in the sample,
/// so a capture with junk before its first packet still counts. Loose on
/// purpose: a false "yes" only sends the file to ffprobe.
fn has_ts_packets(b: &[u8]) -> bool {
    [188usize, 192, 204].iter().any(|&stride| {
        let starts = b.len().saturating_sub(2 * stride);
        (0..starts).any(|s| b[s] == 0x47 && b[s + stride] == 0x47 && b[s + 2 * stride] == 0x47)
    })
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
    fn text_and_packetless_ts_names_are_not_media() {
        let ts = ContainerHint::from_extension("ts");
        let mkv = ContainerHint::from_extension("mkv");
        // TypeScript sources (and their declaration files) are text.
        let code = b"import { a } from './a';\nexport declare function f(x: number): string;\r\n\t// \xe7\xa4\xba\xe4\xbe\x8b\n";
        assert_eq!(not_media(code, ts), Some("a text file, not a video"));
        assert_eq!(
            not_media(b"not really a movie", mkv),
            Some("a text file, not a video")
        );
        // A UTF-8 sequence cut off by the sample's end is still text.
        assert!(not_media(&code[..code.len() - 2], ts).is_some());
        // Binary that ffprobe may still read is left to it…
        let mut junk: Vec<u8> = (0..4096u32).map(|i| (i * 7 % 251) as u8).collect();
        junk[0] = 0;
        assert_eq!(not_media(&junk, mkv), None);
        assert_eq!(not_media(b"FLV\x01\x05\0\0\0\x09", ts), None);
        assert_eq!(not_media(&[0, 0, 1, 0xba, 0x44, 0, 4], ts), None);
        // …but a TS name needs TS packets.
        assert!(not_media(&junk, ts).unwrap().contains("0x47"));
        assert_eq!(not_media(&[], ts), None);
        for (stride, lead) in [(188, 0), (192, 4), (204, 0)] {
            let mut p = vec![0u8; stride * 4 + 333];
            for k in 0..4 {
                p[333 + lead + k * stride] = 0x47;
            }
            assert_eq!(not_media(&p, ts), None, "stride {stride}");
        }
        // Packets after a long binary lead-in (a capture's junk) still count.
        let mut late = junk.repeat(2);
        late[1] = 0x01;
        assert!(not_media(&late, ts).is_some());
        for k in 0..3 {
            late[6000 + k * 188] = 0x47;
        }
        assert_eq!(not_media(&late, ts), None);
    }

    #[test]
    fn hint() {
        assert_eq!(ContainerHint::from_extension("M2TS").0, Some(Container::M2ts));
        assert_eq!(
            ContainerHint::from_path(Path::new("a/b.txt")),
            ContainerHint::NONE
        );
    }
}
