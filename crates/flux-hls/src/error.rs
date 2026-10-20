//! Error type for the playback engine.

use std::io;

/// Everything that can go wrong while indexing, planning or producing segments.
#[derive(Debug, thiserror::Error)]
pub enum HlsError {
    /// Reading the source file failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The container is malformed or truncated where we needed it.
    #[error("malformed {container}: {detail}")]
    Malformed {
        /// "Matroska", "MP4", …
        container: &'static str,
        /// What exactly was wrong.
        detail: String,
    },

    /// The file is valid but uses something this engine cannot remux
    /// (e.g. zlib-compressed Matroska blocks, a container without an index).
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A request referenced a segment, track or rendition that does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// An ffmpeg child process failed.
    #[error("ffmpeg failed: {0}")]
    Ffmpeg(String),

    /// A cached index could not be (de)serialized.
    #[error("index cache: {0}")]
    Cache(#[from] serde_json::Error),

    /// The operation was cancelled (client went away, engine shut down).
    #[error("cancelled")]
    Cancelled,
}

impl HlsError {
    pub(crate) fn mkv(detail: impl Into<String>) -> Self {
        HlsError::Malformed {
            container: "Matroska",
            detail: detail.into(),
        }
    }
    pub(crate) fn mp4(detail: impl Into<String>) -> Self {
        HlsError::Malformed {
            container: "MP4",
            detail: detail.into(),
        }
    }
}

/// Result alias for this crate.
pub type Result<T, E = HlsError> = std::result::Result<T, E>;
