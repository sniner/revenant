pub mod systemd_boot;

use std::path::PathBuf;

use crate::error::Result;
use crate::snapshot::SnapshotId;

/// Information about a boot loader entry.
#[derive(Debug, Clone)]
pub struct BootEntry {
    pub id: String,
    pub title: String,
    pub path: PathBuf,
}

/// Abstraction over boot loader operations.
pub trait BootloaderBackend: Send + Sync {
    /// Check if this bootloader is active on the system.
    fn detect(&self) -> Result<bool>;

    /// Get the path to the EFI system partition.
    fn efi_partition_path(&self) -> Result<PathBuf>;

    /// List all boot entries.
    fn list_entries(&self) -> Result<Vec<BootEntry>>;

    /// Create a rollback boot entry for a snapshot.
    fn create_rollback_entry(&self, snapshot_id: &SnapshotId, rootfs_subvol: &str) -> Result<()>;

    /// Remove a rollback boot entry.
    fn remove_rollback_entry(&self, snapshot_id: &SnapshotId) -> Result<()>;
}
