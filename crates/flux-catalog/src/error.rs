//! Error type shared by every catalog operation.

/// Everything that can go wrong in the catalog.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The database was written by a newer flux; refuse to touch it rather
    /// than corrupt it.
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: i64, supported: i64 },

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid argument: {0}")]
    Invalid(String),

    /// Two roots of one library would overlap: the same folder, or one
    /// inside the other. Every file below the inner root would be cataloged
    /// twice (one row per root, two assets on one movie). Across libraries
    /// see [`CatalogError::RootInOtherLibrary`].
    #[error("root {root:?} overlaps root {other:?} of the same library (one contains the other)")]
    OverlappingRoots { root: String, other: String },

    /// A new root is a folder another library already has (`other`, of
    /// `library`), or one inside or around it: every file below would be
    /// listed in both libraries. The message is a sentence for people.
    #[error("{}", root_in_other_library(.root, .other, .library, .how))]
    RootInOtherLibrary {
        root: String,
        other: String,
        library: String,
        how: RootOverlap,
    },

    /// A value stored in the database is not one this build understands
    /// (e.g. an unknown enum string). Indicates corruption or a downgrade.
    #[error("corrupt row: {0}")]
    Corrupt(String),

    /// The writer thread has exited (shutdown, or the channel was dropped).
    #[error("catalog writer has shut down")]
    WriterGone,

    /// The job itself succeeded but the surrounding transaction failed to
    /// commit, so its effects were rolled back.
    #[error("commit failed: {0}")]
    CommitFailed(String),

    /// Part of a scan batch that [`crate::Writer::apply`] split into several
    /// jobs failed (that part was rolled back; the cursor was not saved).
    #[error("scan batch partly failed: {0}")]
    BatchFailed(String),

    /// A writer job panicked; its savepoint was rolled back.
    #[error("writer job panicked: {0}")]
    JobPanicked(String),

    /// A blocking read task could not be joined (runtime shutting down).
    #[error("read task failed: {0}")]
    ReadTask(String),
}

/// How one root folder relates to another (see
/// [`crate::writer::root_overlap`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootOverlap {
    /// The same folder (perhaps spelled differently).
    Same,
    /// Inside the other folder.
    Inside,
    /// Around the other folder.
    Contains,
}

fn root_in_other_library(root: &str, other: &str, library: &str, how: &RootOverlap) -> String {
    let what = match how {
        RootOverlap::Same if root == other => {
            format!("The folder \"{root}\" is already in the library \"{library}\".")
        }
        RootOverlap::Same => {
            format!("The folder \"{root}\" is already in the library \"{library}\" (as \"{other}\").")
        }
        RootOverlap::Inside => {
            format!(
                "The folder \"{root}\" is inside \"{other}\", which is already in the library \"{library}\"."
            )
        }
        RootOverlap::Contains => {
            format!(
                "The folder \"{root}\" contains \"{other}\", which is already in the library \"{library}\"."
            )
        }
    };
    format!("{what} A folder can be in only one library, or its files would be listed twice.")
}

/// Result alias used across the crate.
pub type Result<T, E = CatalogError> = std::result::Result<T, E>;
