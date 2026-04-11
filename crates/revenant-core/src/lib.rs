#![allow(clippy::missing_errors_doc)]

pub mod backend;
pub mod bootloader;
pub mod check;
pub mod cleanup;
pub mod config;
pub mod efi;
pub mod error;
pub mod init;
pub mod pkgmgr;
pub mod restore;
pub mod retention;
pub mod snapshot;
pub mod systemd;

pub use backend::{FileSystemBackend, SubvolumeInfo};
pub use bootloader::BootloaderBackend;
pub use config::{Config, DELETE_STRAIN, RetainConfig, StrainConfig};
pub use error::{Result, RevenantError};
pub use snapshot::{SnapshotId, SnapshotInfo};
