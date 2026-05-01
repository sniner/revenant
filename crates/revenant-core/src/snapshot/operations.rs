use std::path::Path;

use crate::backend::{FileSystemBackend, subvol_exists};
use crate::config::Config;
use crate::error::{Result, RevenantError};
use crate::metadata::{self, SnapshotMetadata, TriggerKind};

use super::discovery::discover_snapshots;
use super::id::SnapshotId;
use super::info::{SnapshotInfo, ensure_snapshot_dir, sidecar_path_for_snapshot, snapshot_dir};

/// Orchestrate a full snapshot creation for a given strain.
///
/// Does not touch DELETE markers: they are managed exclusively by
/// `apply_retention` / `revenantctl cleanup`, so a marker left over by
/// a prior restore survives across boot-time and periodic snapshots
/// until the user explicitly runs cleanup (or the next restore, which
/// would refuse to collide on the marker path).
pub fn create_snapshot(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    strain_name: &str,
    trigger: TriggerKind,
    message: Vec<String>,
) -> Result<SnapshotInfo> {
    let strain_config = config.strain(strain_name)?;
    let id = SnapshotId::now();
    let snap_dir = ensure_snapshot_dir(config, backend, toplevel)?;
    tracing::info!("creating snapshot {id} (strain: {strain_name})");

    let mut snapshotted_subvols = Vec::new();

    // Snapshot all subvolumes in this strain
    for subvol in &strain_config.subvolumes {
        let src = toplevel.join(subvol);
        let dest = snap_dir.join(id.snapshot_name(subvol, strain_name));
        tracing::info!("snapshotting {subvol} → {}", dest.display());
        backend.create_readonly_snapshot(&src, &dest)?;
        snapshotted_subvols.push(subvol.clone());
    }

    // EFI sync
    let efi_synced = if strain_config.efi && config.sys.efi.enabled {
        let staging = &config.sys.efi.staging_subvol;
        let staging_path = toplevel.join(staging);

        // Ensure staging subvolume exists
        if !subvol_exists(backend, &staging_path) {
            tracing::info!("creating EFI staging subvolume {staging}");
            backend.create_subvolume(&staging_path)?;
            // Initial sync from ESP to staging
            crate::efi::sync_to_staging(&config.sys.efi.mount_point, &staging_path)?;
        }

        // Create a writable snapshot for syncing (temporary, in toplevel)
        let tmp_snap = toplevel.join(format!("{}-rw-tmp", id.snapshot_name(staging, strain_name)));
        backend.create_writable_snapshot(&staging_path, &tmp_snap)?;

        // Sync current ESP content into the writable snapshot
        crate::efi::sync_to_staging(&config.sys.efi.mount_point, &tmp_snap)?;

        // Create the final readonly snapshot in snapshot dir
        let final_snap = snap_dir.join(id.snapshot_name(staging, strain_name));
        backend.create_readonly_snapshot(&tmp_snap, &final_snap)?;

        // Remove temporary writable snapshot
        backend.delete_subvolume(&tmp_snap)?;

        snapshotted_subvols.push(staging.clone());
        true
    } else {
        false
    };

    let mut info = SnapshotInfo {
        id,
        strain: strain_name.to_string(),
        subvolumes: snapshotted_subvols,
        efi_synced,
        metadata: None,
    };

    // Best-effort sidecar write: the subvolumes already exist, so metadata
    // loss is preferable to failing a snapshot that is otherwise intact.
    let metadata = SnapshotMetadata::new(trigger, message);
    let sidecar = sidecar_path_for_snapshot(&snap_dir, strain_name, &info.id);
    match metadata::write(&sidecar, &metadata) {
        Ok(()) => info.metadata = Some(metadata),
        Err(e) => tracing::warn!("failed to write metadata {}: {e}", sidecar.display()),
    }

    tracing::info!("snapshot {} created successfully", info.id);
    Ok(info)
}

/// Delete a snapshot and all its associated subvolumes.
///
/// Refuses to touch snapshots whose sidecar carries `protected = true`;
/// in that case nothing is removed and a `ProtectedSnapshot` error is
/// returned so the caller can surface the blocking message verbatim.
/// Retention also pre-filters protected snapshots; this check is
/// defence-in-depth for direct callers (CLI `delete`, GUI delete button).
pub fn delete_snapshot(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    snapshot: &SnapshotInfo,
) -> Result<()> {
    if snapshot.metadata.as_ref().is_some_and(|m| m.protected) {
        return Err(RevenantError::ProtectedSnapshot {
            strain: snapshot.strain.clone(),
            id: snapshot.id.to_string(),
        });
    }

    tracing::info!(
        "deleting snapshot {} (strain: {})",
        snapshot.id,
        snapshot.strain
    );

    let snap_dir = snapshot_dir(config, toplevel);
    for subvol in &snapshot.subvolumes {
        let snap_path = snap_dir.join(snapshot.id.snapshot_name(subvol, &snapshot.strain));
        if subvol_exists(backend, &snap_path) {
            tracing::info!("deleting subvolume {}", snap_path.display());
            backend.delete_subvolume(&snap_path)?;
        }
    }

    let sidecar = sidecar_path_for_snapshot(&snap_dir, &snapshot.strain, &snapshot.id);
    if let Err(e) = metadata::remove(&sidecar) {
        tracing::warn!("failed to remove metadata {}: {e}", sidecar.display());
    }

    tracing::info!("snapshot {} deleted", snapshot.id);
    Ok(())
}

/// Outcome of `delete_all_strain`: ids that were removed and ids that
/// were skipped because the snapshot is protected. Both are returned
/// (rather than letting the protected case error out) so a bulk delete
/// over a mixed strain still cleans up everything it can without
/// surprising the user with a half-finished operation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BulkDeleteOutcome {
    pub deleted: Vec<String>,
    pub skipped_protected: Vec<String>,
}

/// Field-level patch for an existing snapshot's sidecar. `None` means
/// "leave this field alone". An empty `Some(vec![])` for `message`
/// clears the message list.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MetadataPatch {
    pub protected: Option<bool>,
    pub message: Option<Vec<String>>,
}

impl MetadataPatch {
    /// `true` when no field is set — the caller has nothing to apply.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.protected.is_none() && self.message.is_none()
    }
}

/// Apply `patch` to a snapshot's sidecar in place and return the
/// updated metadata. Errors if no sidecar exists for the snapshot —
/// mutating an absent file would silently materialise metadata that
/// the user did not previously have.
pub fn update_snapshot_metadata(
    config: &Config,
    toplevel: &Path,
    snapshot: &SnapshotInfo,
    patch: &MetadataPatch,
) -> Result<SnapshotMetadata> {
    let snap_dir = snapshot_dir(config, toplevel);
    let sidecar = sidecar_path_for_snapshot(&snap_dir, &snapshot.strain, &snapshot.id);

    let mut meta = metadata::read(&sidecar)?.ok_or_else(|| {
        RevenantError::Other(format!(
            "snapshot {}@{} has no sidecar metadata to edit",
            snapshot.strain, snapshot.id
        ))
    })?;

    if let Some(p) = patch.protected {
        meta.protected = p;
    }
    if let Some(msgs) = &patch.message {
        meta.message = msgs.clone();
    }

    metadata::write(&sidecar, &meta)?;
    Ok(meta)
}

/// Delete all snapshots belonging to a given strain. Protected snapshots
/// are silently skipped and reported back via
/// `BulkDeleteOutcome::skipped_protected`; the caller is expected to
/// surface a warning.
pub fn delete_all_strain(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    strain_name: &str,
) -> Result<BulkDeleteOutcome> {
    let all = discover_snapshots(config, backend, toplevel)?;
    let (deletable, skipped): (Vec<_>, Vec<_>) = all
        .into_iter()
        .filter(|s| s.strain == strain_name)
        .partition(|s| !s.metadata.as_ref().is_some_and(|m| m.protected));

    let mut outcome = BulkDeleteOutcome {
        deleted: Vec::with_capacity(deletable.len()),
        skipped_protected: skipped.iter().map(|s| s.id.to_string()).collect(),
    };

    for snap in &deletable {
        tracing::info!("deleting snapshot {} (strain: {strain_name})", snap.id);
        delete_snapshot(config, backend, toplevel, snap)?;
        outcome.deleted.push(snap.id.to_string());
    }

    if !outcome.skipped_protected.is_empty() {
        tracing::info!(
            "skipped {} protected snapshot(s) in strain {strain_name}",
            outcome.skipped_protected.len()
        );
    }

    Ok(outcome)
}
