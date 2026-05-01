use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::backend::{FileSystemBackend, subvol_exists};
use crate::config::Config;
use crate::error::Result;
use crate::metadata::{self, SnapshotMetadata};

use super::id::SnapshotId;

/// Render `(strain, id)` as the canonical `strain@id` token used in
/// human-facing output (replaces the older `(strain: …)` parenthetical).
#[must_use]
pub fn qualified(strain: &str, id: &SnapshotId) -> String {
    format!("{strain}@{id}")
}

/// A discovered snapshot, derived from scanning actual subvolumes on disk.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    pub id: SnapshotId,
    pub strain: String,
    /// Subvolumes found for this snapshot (e.g. `["@", "@boot"]`).
    pub subvolumes: Vec<String>,
    /// Whether the EFI staging subvolume snapshot is present.
    pub efi_synced: bool,
    /// Optional sidecar metadata (trigger, message, …). `None` means no
    /// sidecar file was found or it could not be parsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SnapshotMetadata>,
}

/// Return the path to the snapshot subvolume within the toplevel.
pub(super) fn snapshot_dir(config: &Config, toplevel: &Path) -> PathBuf {
    toplevel.join(&config.sys.snapshot_subvol)
}

/// Compute the sidecar metadata path for a snapshot. The sidecar is
/// keyed on `(strain, id)` only, so reordering the strain's
/// `subvolumes = [...]` list does not orphan existing metadata.
pub(super) fn sidecar_path_for_snapshot(snap_dir: &Path, strain: &str, id: &SnapshotId) -> PathBuf {
    metadata::sidecar_path(snap_dir, strain, id.as_str())
}

/// Ensure the snapshot subvolume exists, creating it if necessary.
pub(super) fn ensure_snapshot_dir(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<PathBuf> {
    let dir = snapshot_dir(config, toplevel);
    if !subvol_exists(backend, &dir) {
        tracing::info!("creating snapshot subvolume {}", config.sys.snapshot_subvol);
        backend.create_subvolume(&dir)?;
    }
    Ok(dir)
}
