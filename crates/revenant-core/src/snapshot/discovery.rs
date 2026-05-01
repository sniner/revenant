use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use crate::backend::{FileSystemBackend, subvol_exists};
use crate::config::Config;
use crate::error::{Result, RevenantError};
use crate::metadata;

use super::id::SnapshotId;
use super::info::{SnapshotInfo, qualified, sidecar_path_for_snapshot, snapshot_dir};

/// Discover all snapshots by scanning subvolumes on disk and matching against config.
///
/// For each configured strain and its subvolumes, looks for subvolume names matching
/// `{subvol}-{strain}-{id}` (where `id` is `YYYYMMDD-HHMMSS-NNN` or the legacy
/// `YYYYMMDD-HHMMSS`), groups by (strain, id), and returns the list sorted
/// chronologically.
pub fn discover_snapshots(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<Vec<SnapshotInfo>> {
    // List actual subvolumes in the snapshot directory
    let snap_dir = snapshot_dir(config, toplevel);
    if !subvol_exists(backend, &snap_dir) {
        return Ok(Vec::new());
    }
    let subvols = backend.list_subvolumes(&snap_dir)?;
    let entries: Vec<&str> = subvols
        .iter()
        .filter_map(|s| s.path.file_name().and_then(|n| n.to_str()))
        .collect();

    // Key: (strain, timestamp) → set of found subvol base names
    let mut found: HashMap<(String, String), Vec<String>> = HashMap::new();

    for (strain_name, strain_config) in &config.strain {
        // Check regular subvolumes
        for subvol in &strain_config.subvolumes {
            let prefix = format!("{subvol}-{strain_name}-");
            for entry in &entries {
                if let Some(rest) = entry.strip_prefix(&prefix) {
                    if SnapshotId::from_string(rest).is_ok() {
                        found
                            .entry((strain_name.clone(), rest.to_string()))
                            .or_default()
                            .push(subvol.clone());
                    }
                }
            }
        }

        // Check EFI staging subvolume
        if strain_config.efi && config.sys.efi.enabled {
            let staging = &config.sys.efi.staging_subvol;
            let prefix = format!("{staging}-{strain_name}-");
            for entry in &entries {
                if let Some(rest) = entry.strip_prefix(&prefix) {
                    if SnapshotId::from_string(rest).is_ok() {
                        found
                            .entry((strain_name.clone(), rest.to_string()))
                            .or_default()
                            .push(staging.clone());
                    }
                }
            }
        }
    }

    // Build SnapshotInfo from discovered data
    let mut snapshots: Vec<SnapshotInfo> = found
        .into_iter()
        .filter_map(|((strain, ts), subvols)| {
            let id = SnapshotId::from_string(&ts).ok()?;
            let efi_synced = config.sys.efi.enabled
                && config
                    .strain
                    .get(&strain)
                    .is_some_and(|sc| sc.efi && subvols.contains(&config.sys.efi.staging_subvol));
            let metadata = {
                let p = sidecar_path_for_snapshot(&snap_dir, &strain, &id);
                match metadata::read(&p) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("ignoring unreadable metadata {}: {e}", p.display());
                        None
                    }
                }
            };
            Some(SnapshotInfo {
                id,
                strain,
                subvolumes: subvols,
                efi_synced,
                metadata,
            })
        })
        .collect();

    snapshots.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.strain.cmp(&b.strain)));
    Ok(snapshots)
}

/// Reference to the snapshot from which the currently live rootfs
/// subvolume was cloned. Resolved from btrfs' `parent_uuid` chain at
/// read time, never persisted: nothing in the sidecar knows about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveParentRef {
    pub id: SnapshotId,
    pub strain: String,
}

/// Identify the snapshot whose subvolume is the btrfs parent of the
/// currently live rootfs subvolume.
///
/// Mechanic: `restore_snapshot` builds the new live subvol via
/// `create_writable_snapshot(snap, live)`, so afterwards
/// `live.parent_uuid == snap.uuid`. On a pristine system the live subvol
/// has no parent uuid and we return `None`.
///
/// Only the strain's rootfs subvolume is consulted. Partial per-subvol
/// restores that leave `@home`/`@boot` on an unrelated lineage are not
/// reflected — the rootfs is the canonical anchor.
///
/// All backend errors are non-fatal: they are logged and the function
/// returns `None`, so `revenantctl list` never refuses to run because
/// the anchor could not be resolved.
pub fn resolve_live_parent(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Option<LiveParentRef> {
    let rootfs_path = toplevel.join(&config.sys.rootfs_subvol);
    let live = match backend.subvolume_info(&rootfs_path) {
        Ok(info) => info,
        Err(e) => {
            tracing::warn!(
                "cannot read rootfs subvolume info at {}: {e}",
                rootfs_path.display()
            );
            return None;
        }
    };
    let parent_uuid = live.parent_uuid?;

    let snap_dir = snapshot_dir(config, toplevel);
    if !subvol_exists(backend, &snap_dir) {
        return None;
    }
    let subvols = match backend.list_subvolumes(&snap_dir) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "cannot list snapshot subvolumes at {}: {e}",
                snap_dir.display()
            );
            return None;
        }
    };

    let prefix = format!("{}-", config.sys.rootfs_subvol);
    for sv in &subvols {
        if sv.uuid != parent_uuid {
            continue;
        }
        let Some(name) = sv.path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some((id, id_start)) = SnapshotId::extract_trailing(rest) else {
            continue;
        };
        let strain = rest[..id_start - 1].to_string();
        return Some(LiveParentRef { id, strain });
    }
    None
}

/// Find a specific snapshot by ID. If strain is None and the ID is ambiguous
/// (exists in multiple strains), returns an error.
pub fn find_snapshot(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    id: &SnapshotId,
    strain: Option<&str>,
) -> Result<SnapshotInfo> {
    let all = discover_snapshots(config, backend, toplevel)?;
    let mut matches: Vec<_> = all
        .into_iter()
        .filter(|s| s.id == *id && strain.is_none_or(|st| s.strain == st))
        .collect();

    match matches.len() {
        0 => Err(RevenantError::SnapshotNotFound(id.to_string())),
        1 => Ok(matches.remove(0)),
        _ => {
            let qualified: Vec<_> = matches
                .iter()
                .map(|s| qualified(&s.strain, &s.id))
                .collect();
            Err(RevenantError::Other(format!(
                "snapshot {id} exists in multiple strains — qualify it: {}",
                qualified.join(", ")
            )))
        }
    }
}
