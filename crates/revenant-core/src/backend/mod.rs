pub mod btrfs;

#[cfg(test)]
pub mod mock;

use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Result;

/// Information about a subvolume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubvolumeInfo {
    pub id: u64,
    pub parent_id: u64,
    pub path: std::path::PathBuf,
    pub uuid: Uuid,
    pub readonly: bool,
}

/// Returns `true` if the given path is a subvolume on the backend.
///
/// Prefer this over `path.exists()` for "is this a subvolume yet" checks:
/// `path.exists()` is true for any directory, including a non-subvolume
/// directory that happens to share the name. Asking the backend distinguishes
/// the two cleanly and is also testable through a mock backend.
#[must_use]
pub fn subvol_exists(backend: &dyn FileSystemBackend, path: &Path) -> bool {
    backend.subvolume_info(path).is_ok()
}

/// Abstraction over copy-on-write filesystem operations.
pub trait FileSystemBackend: Send + Sync {
    /// Check if the given path resides on a supported filesystem.
    fn probe(&self, path: &Path) -> Result<bool>;

    /// List all subvolumes under the given root.
    fn list_subvolumes(&self, root: &Path) -> Result<Vec<SubvolumeInfo>>;

    /// Create a readonly snapshot of `source` at `dest`.
    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> Result<SubvolumeInfo>;

    /// Create a writable snapshot of `source` at `dest`.
    fn create_writable_snapshot(&self, source: &Path, dest: &Path) -> Result<SubvolumeInfo>;

    /// Create a new empty subvolume at `path`.
    fn create_subvolume(&self, path: &Path) -> Result<()>;

    /// Delete a subvolume or snapshot at `path`.
    fn delete_subvolume(&self, path: &Path) -> Result<()>;

    /// Rename a subvolume from `source` to `dest`.
    ///
    /// On btrfs, renaming a subvolume is just renaming its directory entry —
    /// the same as `mv`. Exposed through the trait so orchestration code
    /// (notably `restore_snapshot`'s DELETE-marker step) can be tested
    /// against a mock backend without touching the real filesystem.
    fn rename_subvolume(&self, source: &Path, dest: &Path) -> Result<()>;

    /// Get information about a subvolume at `path`.
    fn subvolume_info(&self, path: &Path) -> Result<SubvolumeInfo>;

    /// Set the default subvolume for the filesystem.
    fn set_default_subvolume(&self, path: &Path) -> Result<()>;

    /// Create a directory at `path`, including any missing parent
    /// directories.  Mirrors `std::fs::create_dir_all`.
    ///
    /// Used by `restore_snapshot` to materialise the parent path of a
    /// nested subvolume being re-attached when the restored snapshot
    /// pre-dates the nested subvolume's creation — the snapshot may not
    /// contain the directory tree that leads up to where the nested
    /// subvolume currently lives, so we have to create it ourselves
    /// before the rename can land.  Without this, rolling back to an
    /// older snapshot would strand the nested data in the DELETE marker.
    fn create_dir_all(&self, path: &Path) -> Result<()>;

    /// Find subvolumes nested directly inside `root`.
    ///
    /// Walks the directory tree under `root` but stops at every subvolume
    /// boundary it finds — so the returned paths are the *direct* nested
    /// children. To walk the full hierarchy, recurse on each result.
    ///
    /// Returns an empty vector if `root` has no nested subvolumes.
    fn find_nested_subvolumes(&self, root: &Path) -> Result<Vec<std::path::PathBuf>>;
}
