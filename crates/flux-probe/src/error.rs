//! Probe errors.

use flux_core::Container;

/// Why a probe failed. A probe never panics: truncated, empty and garbage
/// files all end up here.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The underlying range read failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Zero-byte file.
    #[error("empty file")]
    Empty,
    /// Magic bytes match no container flux-probe understands.
    #[error("unrecognised container (first bytes {magic})")]
    UnknownFormat {
        /// Hex of the first few bytes, for diagnostics.
        magic: String,
    },
    /// The structure points past the end of the file.
    #[error(
        "{container:?} file truncated: needed {needed} bytes at offset {offset}, file is {file_size} bytes"
    )]
    Truncated {
        /// Container being parsed.
        container: Container,
        /// Offset of the missing range.
        offset: u64,
        /// Length of the missing range.
        needed: u64,
        /// Size of the file.
        file_size: u64,
    },
    /// The container structure is inconsistent.
    #[error("malformed {container:?}: {msg}")]
    Malformed {
        /// Container being parsed.
        container: Container,
        /// What was wrong.
        msg: String,
    },
    /// Recognised, but a feature we deliberately don't handle.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The probe asked for more reads/bytes than its budget allows (hostile or
    /// pathological file).
    #[error("read budget exceeded ({reads} reads, {bytes} bytes)")]
    Budget {
        /// Reads so far.
        reads: u32,
        /// Bytes so far.
        bytes: u64,
    },
    /// The I/O driver fed bytes that do not cover the range the prober asked for.
    #[error("driver fed no data at requested offset {0}")]
    BadFeed(u64),
    /// The ffprobe subprocess failed or produced unusable output.
    #[error("ffprobe: {0}")]
    Ffprobe(String),
}

impl ProbeError {
    /// True when ffprobe may still read the file: the container is one
    /// flux-probe doesn't parse, or one it found inconsistent where ffmpeg's
    /// demuxers are more forgiving.
    pub fn wants_ffprobe(&self) -> bool {
        matches!(
            self,
            ProbeError::UnknownFormat { .. } | ProbeError::Malformed { .. }
        )
    }

    pub(crate) fn malformed(container: Container, msg: impl Into<String>) -> Self {
        ProbeError::Malformed {
            container,
            msg: msg.into(),
        }
    }
}
