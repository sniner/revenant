use std::path::PathBuf;

/// Central error type for the revenant-core library.
#[derive(Debug, thiserror::Error)]
pub enum RevenantError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("btrfs ioctl failed on {path}: {message}: {source}")]
    BtrfsIoctl {
        path: PathBuf,
        message: String,
        source: nix::errno::Errno,
    },

    #[error("subvolume not found: {0}")]
    SubvolumeNotFound(PathBuf),

    #[error("snapshot not found: {0}")]
    SnapshotNotFound(String),

    #[error("incomplete snapshot {id}: missing components: {missing:?}")]
    IncompleteSnapshot { id: String, missing: Vec<String> },

    #[error("filesystem at {path} is not btrfs")]
    NotBtrfs { path: PathBuf },

    #[error("mount error: {0}")]
    Mount(String),

    #[error("EFI sync error: {0}")]
    EfiSync(String),

    #[error("bootloader error: {0}")]
    Bootloader(String),

    #[error("operation requires root privileges")]
    NotRoot,

    #[error(
        "snapshot {strain}@{id} is protected; run `revenantctl edit {strain}@{id} --unprotect` first"
    )]
    ProtectedSnapshot { strain: String, id: String },

    #[error("{0}")]
    Other(String),
}

impl RevenantError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, RevenantError>;
