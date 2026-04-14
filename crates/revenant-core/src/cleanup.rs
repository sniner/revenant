use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;

use crate::backend::{FileSystemBackend, subvol_exists};
use crate::config::{Config, DELETE_STRAIN, RetainConfig};
use crate::error::Result;
use crate::metadata;
use crate::retention::{KeepReason, select_to_keep, select_to_keep_explained};
use crate::snapshot::{self, SnapshotId, SnapshotInfo};

/// What the retention policy would do, without touching the filesystem.
///
/// Built by [`plan_retention`] and rendered by the CLI's `--dry-run` path.
/// The fields are serialisable so the same structure can be emitted as
/// JSON in a future `--json` mode.
#[derive(Debug, Clone, Serialize)]
pub struct RetentionPlan {
    /// One entry per configured strain, sorted by strain name for stable output.
    pub strains: Vec<StrainPlan>,
    /// Structured view of any `<base>-DELETE-<ts>` markers that a real
    /// cleanup run would purge.  May be empty.
    pub delete_markers: Vec<DeleteMarker>,
}

/// A parsed `<base>-DELETE-<ts>` marker.
///
/// Markers are the renamed pre-restore live subvolumes — they survive
/// until an explicit `revenantctl cleanup` (or a subsequent restore)
/// removes them.  A dry-run plan carries them as a structured value
/// rather than a raw subvolume name so JSON consumers don't have to
/// parse the `base-DELETE-timestamp` convention themselves.
#[derive(Debug, Clone, Serialize)]
pub struct DeleteMarker {
    /// The live subvol this marker was renamed from, e.g. `"@"`.
    pub base_subvol: String,
    /// Timestamp the marker was created (at the moment of the restore).
    pub id: SnapshotId,
    /// Full subvolume name, e.g. `"@-DELETE-20260411-080055"`.
    pub name: String,
}

/// Retention plan for a single strain.
#[derive(Debug, Clone, Serialize)]
pub struct StrainPlan {
    pub strain: String,
    /// The strain's effective retention rules (echoed so the output is self-contained).
    pub retain: RetainConfig,
    /// Snapshots of this strain, newest-first, annotated with keep/delete.
    pub entries: Vec<PlanEntry>,
}

/// One row in a [`StrainPlan`].
#[derive(Debug, Clone, Serialize)]
pub struct PlanEntry {
    pub id: SnapshotId,
    #[serde(flatten)]
    pub action: PlanAction,
}

/// Decision and justification for a single snapshot in the plan.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum PlanAction {
    /// Snapshot is protected by one or more retention rules.
    Keep { reasons: Vec<KeepReason> },
    /// Snapshot matches no retention rule and would be removed.
    Delete,
}

/// Try to rescue nested subvolumes that ended up stranded inside a
/// `<base>-DELETE-<ts>` marker — typically because a previous `restore`
/// was interrupted between the rename of the live subvolume and the
/// nested-subvolume re-attach step (e.g. power loss after the rename
/// but before the loop that moves the nested subvols into the freshly
/// restored subvolume completed).
///
/// For each marker that contains nested subvolumes, attempt to move
/// those nested subvolumes back into the corresponding live `<base>`
/// subvolume at their original relative paths.  Per-entry failures
/// (live subvol missing, destination already exists as a subvolume,
/// rename failed) are logged as warnings — the marker is left in place
/// so the user or a later run can investigate, and the function never
/// errors out so the surrounding write command can still proceed.
///
/// Designed to be called at the start of every CLI command that
/// modifies on-disk state.  Read-only commands (`list`, `status`,
/// `check`) intentionally do not call this so a stuck recovery does
/// not produce noise on every shell prompt.
///
/// Returns the number of nested subvolumes that were successfully
/// re-attached.
pub fn recover_orphaned_nested_subvols(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<usize> {
    // Same base-name set as `purge_delete_pending_all`: only markers
    // whose prefix matches a known strain subvol (or the EFI staging
    // subvol) are considered ours to touch.
    let mut base_names: HashSet<&str> = HashSet::new();
    for sc in config.strain.values() {
        for sv in &sc.subvolumes {
            base_names.insert(sv.as_str());
        }
    }
    if config.sys.efi.enabled {
        base_names.insert(config.sys.efi.staging_subvol.as_str());
    }

    let all_subvols = backend.list_subvolumes(toplevel)?;
    let mut recovered = 0usize;

    for entry in all_subvols {
        let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Find which base this marker belongs to.  We need the base
        // name to figure out which live subvol the orphans should
        // move back into.
        let Some(base) = base_names
            .iter()
            .find(|b| name.starts_with(&format!("{b}-{DELETE_STRAIN}-")))
            .copied()
        else {
            continue;
        };

        let nested = match backend.find_nested_subvolumes(&entry.path) {
            Ok(n) if !n.is_empty() => n,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(
                    "could not scan {} for nested subvolumes: {e}",
                    entry.path.display()
                );
                continue;
            }
        };

        let live_path = toplevel.join(base);
        if !subvol_exists(backend, &live_path) {
            tracing::warn!(
                "DELETE marker {} contains {} nested subvolume(s) but live subvol {} does not exist; leaving marker in place for manual review",
                name,
                nested.len(),
                live_path.display()
            );
            continue;
        }

        for nested_path in &nested {
            let Ok(rel) = nested_path.strip_prefix(&entry.path) else {
                continue;
            };
            let to = live_path.join(rel);

            if subvol_exists(backend, &to) {
                tracing::warn!(
                    "skipping recovery of {} → {}: destination already exists as a subvolume; leaving in DELETE marker for manual review",
                    nested_path.display(),
                    to.display()
                );
                continue;
            }

            if let Some(parent) = to.parent() {
                if let Err(e) = backend.create_dir_all(parent) {
                    tracing::warn!(
                        "could not materialise parent path {} for recovery: {e}",
                        parent.display()
                    );
                    continue;
                }
            }

            tracing::info!(
                "recovering orphaned nested subvolume {} → {}",
                nested_path.display(),
                to.display()
            );
            match backend.rename_subvolume(nested_path, &to) {
                Ok(()) => recovered += 1,
                Err(e) => {
                    tracing::warn!(
                        "could not recover {} → {}: {e}",
                        nested_path.display(),
                        to.display()
                    );
                }
            }
        }
    }

    Ok(recovered)
}

/// Delete all subvolumes carrying the DELETE marker across all configured subvol names.
///
/// Called automatically by `apply_retention` (i.e. `revenantctl cleanup`).
/// `create_snapshot` deliberately does *not* call this any more: DELETE
/// markers are the volatile undo buffer for the previous live state after
/// a restore, and purging them on every boot-time snapshot would defeat
/// that purpose.  Failures per entry are logged as warnings; the function
/// always succeeds so that a mounted subvolume (live root before reboot)
/// does not block normal operation.
pub fn purge_delete_pending_all(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<Vec<String>> {
    // Collect all base subvol names known to the config
    let mut base_names: HashSet<&str> = HashSet::new();
    for strain_config in config.strain.values() {
        for sv in &strain_config.subvolumes {
            base_names.insert(sv.as_str());
        }
    }
    if config.sys.efi.enabled {
        base_names.insert(config.sys.efi.staging_subvol.as_str());
    }

    let all_subvols = backend.list_subvolumes(toplevel)?;
    let mut removed = Vec::new();

    for entry in all_subvols {
        if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
            let matched = base_names
                .iter()
                .any(|base| name.starts_with(&format!("{base}-{DELETE_STRAIN}-")));
            if matched {
                tracing::info!("cleanup: purging DELETE-marked subvol '{name}'");
                match delete_subvolume_recursive(backend, &entry.path) {
                    Ok(()) => removed.push(name.to_string()),
                    Err(e) => tracing::warn!(
                        "could not delete '{name}': {e} (will be retried on next run)"
                    ),
                }
            }
        }
    }

    Ok(removed)
}

/// Delete a subvolume and any subvolumes nested inside it.
///
/// Used by DELETE-marker cleanup: when a restore renames the live `@`
/// subvolume to `@-DELETE-{ts}`, any nested subvolumes that lived inside
/// the original `@` (e.g. `@/var/lib/portables` on a stock Arch install)
/// move along with it. btrfs `SNAP_DESTROY` refuses to delete a subvolume
/// that still contains nested subvolumes (ENOTEMPTY), so we have to walk
/// the marker depth-first and clear it from the inside out.
///
/// This is intentionally only invoked for DELETE markers — never for live
/// subvolumes or snapshots, where dropping nested data would be a
/// destructive surprise. The `check` command warns about nested
/// subvolumes precisely because rollback discards them; this function is
/// the place where that discard actually happens.
fn delete_subvolume_recursive(backend: &dyn FileSystemBackend, path: &Path) -> Result<()> {
    for child in backend.find_nested_subvolumes(path)? {
        delete_subvolume_recursive(backend, &child)?;
    }
    backend.delete_subvolume(path)
}

/// List all `<base>-DELETE-<ts>` markers at the top level that a subsequent
/// `purge_delete_pending_all` would remove, as structured [`DeleteMarker`]s.
///
/// Used by `plan_retention` for the dry-run report.  Shares the same
/// base-name set as `purge_delete_pending_all` / `recover_orphaned_nested_subvols`
/// so that the three functions agree on what counts as "ours".
///
/// Markers whose timestamp suffix does not parse as a valid [`SnapshotId`]
/// are dropped with a warning — they may be leftovers from a prior tool
/// version or a hand-crafted name, and presenting them as numbered
/// structured data would be dishonest.  A real cleanup run still picks
/// them up because `purge_delete_pending_all` only matches the `<base>-DELETE-`
/// prefix and does not parse the suffix.
fn find_delete_markers(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<Vec<DeleteMarker>> {
    let mut base_names: HashSet<&str> = HashSet::new();
    for strain_config in config.strain.values() {
        for sv in &strain_config.subvolumes {
            base_names.insert(sv.as_str());
        }
    }
    if config.sys.efi.enabled {
        base_names.insert(config.sys.efi.staging_subvol.as_str());
    }

    let all_subvols = backend.list_subvolumes(toplevel)?;
    let mut markers = Vec::new();
    for entry in all_subvols {
        let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((base, suffix)) = base_names.iter().find_map(|base| {
            let prefix = format!("{base}-{DELETE_STRAIN}-");
            name.strip_prefix(&prefix).map(|rest| (*base, rest))
        }) else {
            continue;
        };
        match SnapshotId::from_string(suffix) {
            Ok(id) => markers.push(DeleteMarker {
                base_subvol: base.to_string(),
                id,
                name: name.to_string(),
            }),
            Err(_) => {
                tracing::warn!(
                    "skipping DELETE marker '{name}' in dry-run report: timestamp suffix '{suffix}' is not a valid snapshot ID"
                );
            }
        }
    }
    markers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(markers)
}

/// Compute — without modifying anything on disk — what `apply_retention`
/// would do.  Returns a [`RetentionPlan`] that enumerates every snapshot
/// per configured strain with a keep/delete decision (plus the reasons
/// protecting a kept snapshot) and lists any DELETE markers that would
/// be purged.
///
/// This is the engine behind `revenantctl cleanup --dry-run`.
pub fn plan_retention(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<RetentionPlan> {
    let delete_markers = find_delete_markers(config, backend, toplevel)?;
    let all_snapshots = snapshot::discover_snapshots(config, backend, toplevel)?;

    // Stable strain order — output must not depend on HashMap iteration order.
    let mut strain_names: Vec<&String> = config.strain.keys().collect();
    strain_names.sort();

    let mut strains = Vec::with_capacity(strain_names.len());
    for strain_name in strain_names {
        let strain_config = &config.strain[strain_name];
        let mut strain_snapshots: Vec<&SnapshotInfo> = all_snapshots
            .iter()
            .filter(|s| s.strain == *strain_name)
            .collect();
        // Newest-first for display and for consistency with the bucket logic.
        strain_snapshots.sort_by(|a, b| b.id.cmp(&a.id));

        let keep_map = select_to_keep_explained(&strain_snapshots, &strain_config.retain);

        let entries: Vec<PlanEntry> = strain_snapshots
            .iter()
            .map(|snap| {
                let action = keep_map
                    .get(snap.id.as_str())
                    .map_or(PlanAction::Delete, |reasons| PlanAction::Keep {
                        reasons: reasons.clone(),
                    });
                PlanEntry {
                    id: snap.id.clone(),
                    action,
                }
            })
            .collect();

        strains.push(StrainPlan {
            strain: strain_name.clone(),
            retain: strain_config.retain.clone(),
            entries,
        });
    }

    Ok(RetentionPlan {
        strains,
        delete_markers,
    })
}

/// Remove sidecar metadata files whose matching snapshot subvolume is gone.
///
/// Delegates discovery to [`metadata::find_orphaned_sidecars`] and only
/// handles the remove-per-entry side. Per-entry removal failures are
/// logged and skipped — the command always succeeds so that a single
/// unreadable entry cannot block retention.
pub fn purge_orphaned_sidecars(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<Vec<String>> {
    let snap_dir = toplevel.join(&config.sys.snapshot_subvol);
    let orphans = metadata::find_orphaned_sidecars(&snap_dir, backend)?;

    let mut removed = Vec::new();
    for orphan in orphans {
        match std::fs::remove_file(&orphan.path) {
            Ok(()) => {
                tracing::info!("cleanup: purging orphaned sidecar '{}'", orphan.name);
                removed.push(orphan.name);
            }
            Err(e) => {
                tracing::warn!(
                    "could not delete sidecar '{}': {e} (will be retried on next run)",
                    orphan.name
                );
            }
        }
    }

    Ok(removed)
}

/// Combined result of a live `cleanup` run. Separates subvolume removals
/// (retention drops + purged DELETE markers) from orphaned sidecar file
/// removals so the UI can report each category individually.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CleanupSummary {
    /// Snapshot IDs and DELETE marker names that were removed.
    pub removed: Vec<String>,
    /// Filenames of orphaned sidecar metadata files that were removed.
    pub removed_sidecars: Vec<String>,
}

/// Apply per-strain retention policy: keep only the most recent `retain` snapshots per strain.
///
/// Also purges any DELETE-marked subvolumes left over from previous restores
/// and any orphaned sidecar metadata files. Discovers snapshots on disk,
/// then delegates to `apply_retention_to`.
pub fn apply_retention(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<CleanupSummary> {
    let mut removed = purge_delete_pending_all(config, backend, toplevel)?;
    let all_snapshots = snapshot::discover_snapshots(config, backend, toplevel)?;
    removed.extend(apply_retention_to(
        config,
        backend,
        toplevel,
        &all_snapshots,
    )?);
    let removed_sidecars = purge_orphaned_sidecars(config, backend, toplevel)?;
    Ok(CleanupSummary {
        removed,
        removed_sidecars,
    })
}

/// Apply per-strain retention policy to a pre-discovered list of snapshots.
///
/// Use this when you already have the discovery result to avoid scanning twice.
pub fn apply_retention_to(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    all_snapshots: &[SnapshotInfo],
) -> Result<Vec<String>> {
    let mut removed = Vec::new();

    for (strain_name, strain_config) in &config.strain {
        let strain_snapshots: Vec<&SnapshotInfo> = all_snapshots
            .iter()
            .filter(|s| s.strain == *strain_name)
            .collect();

        let keep = select_to_keep(&strain_snapshots, &strain_config.retain);

        for snap in &strain_snapshots {
            if !keep.contains(snap.id.as_str()) {
                tracing::info!(
                    "retention: removing snapshot {} (strain: {strain_name})",
                    snap.id
                );
                snapshot::delete_snapshot(config, backend, toplevel, snap)?;
                removed.push(snap.id.to_string());
            }
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    /// Build a Config with one strain `default` retaining the newest `last` snapshots.
    fn config_retain_last(subvols: &[&str], last: usize) -> Config {
        let subvol_list = subvols
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let toml = format!(
            r#"
[sys]
rootfs_subvol = "@"
snapshot_subvol = "@snapshots"

[sys.rootfs]
backend = "btrfs"
device_uuid = "12345678-1234-1234-1234-123456789abc"

[sys.efi]
enabled = false

[sys.bootloader]
backend = "systemd-boot"

[strain.default]
subvolumes = [{subvol_list}]

[strain.default.retain]
last = {last}
"#
        );
        toml.parse().unwrap()
    }

    /// Two strains `default` (last=2) and `periodic` (last=1) over the same subvol set.
    fn config_two_strains_retain(subvols: &[&str]) -> Config {
        let subvol_list = subvols
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let toml = format!(
            r#"
[sys]
rootfs_subvol = "@"
snapshot_subvol = "@snapshots"

[sys.rootfs]
backend = "btrfs"
device_uuid = "12345678-1234-1234-1234-123456789abc"

[sys.efi]
enabled = false

[sys.bootloader]
backend = "systemd-boot"

[strain.default]
subvolumes = [{subvol_list}]

[strain.default.retain]
last = 2

[strain.periodic]
subvolumes = [{subvol_list}]

[strain.periodic.retain]
last = 1
"#
        );
        toml.parse().unwrap()
    }

    fn setup_mock(config: &Config, toplevel: &Path) -> MockBackend {
        let mock = MockBackend::new();
        for sc in config.strain.values() {
            for sv in &sc.subvolumes {
                let p = toplevel.join(sv);
                if !mock.contains(&p) {
                    mock.seed_subvolume(p);
                }
            }
        }
        mock.seed_subvolume(toplevel.join(&config.sys.snapshot_subvol));
        mock
    }

    fn seed_snapshot_for(
        mock: &MockBackend,
        config: &Config,
        toplevel: &Path,
        strain: &str,
        id: &str,
    ) {
        let snap_dir = toplevel.join(&config.sys.snapshot_subvol);
        let sc = config.strain.get(strain).unwrap();
        for sv in &sc.subvolumes {
            mock.seed_subvolume(snap_dir.join(format!("{sv}-{strain}-{id}")));
        }
    }

    // ----- apply_retention -----

    #[test]
    fn retention_keeps_newest_n() {
        let config = config_retain_last(&["@"], 2);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        for ts in &[
            "20260101-000000",
            "20260102-000000",
            "20260103-000000",
            "20260104-000000",
        ] {
            seed_snapshot_for(&mock, &config, toplevel, "default", ts);
        }

        let summary = apply_retention(&config, &mock, toplevel).unwrap();
        // Two oldest should be removed
        assert_eq!(summary.removed.len(), 2);
        assert!(summary.removed.contains(&"20260101-000000".to_string()));
        assert!(summary.removed.contains(&"20260102-000000".to_string()));
        assert!(summary.removed_sidecars.is_empty());

        // The two newest still exist on disk
        assert!(mock.contains("/top/@snapshots/@-default-20260103-000000"));
        assert!(mock.contains("/top/@snapshots/@-default-20260104-000000"));
        // Removed ones are gone
        assert!(!mock.contains("/top/@snapshots/@-default-20260101-000000"));
        assert!(!mock.contains("/top/@snapshots/@-default-20260102-000000"));
    }

    #[test]
    fn retention_isolates_strains() {
        // default keeps last=2, periodic keeps last=1.
        let config = config_two_strains_retain(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        for ts in &["20260101-000000", "20260102-000000", "20260103-000000"] {
            seed_snapshot_for(&mock, &config, toplevel, "default", ts);
            seed_snapshot_for(&mock, &config, toplevel, "periodic", ts);
        }

        let summary = apply_retention(&config, &mock, toplevel).unwrap();
        // default: drops 20260101 (keeps 02 + 03)
        // periodic: drops 20260101 + 20260102 (keeps 03)
        assert_eq!(summary.removed.len(), 3);

        // Strain isolation: default's snapshots untouched aside from the
        // single drop, periodic's likewise.
        assert!(mock.contains("/top/@snapshots/@-default-20260102-000000"));
        assert!(mock.contains("/top/@snapshots/@-default-20260103-000000"));
        assert!(!mock.contains("/top/@snapshots/@-default-20260101-000000"));

        assert!(mock.contains("/top/@snapshots/@-periodic-20260103-000000"));
        assert!(!mock.contains("/top/@snapshots/@-periodic-20260101-000000"));
        assert!(!mock.contains("/top/@snapshots/@-periodic-20260102-000000"));
    }

    #[test]
    fn retention_under_limit_removes_nothing() {
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        for ts in &["20260101-000000", "20260102-000000"] {
            seed_snapshot_for(&mock, &config, toplevel, "default", ts);
        }

        let summary = apply_retention(&config, &mock, toplevel).unwrap();
        assert!(summary.removed.is_empty());
        assert!(summary.removed_sidecars.is_empty());
        assert!(mock.contains("/top/@snapshots/@-default-20260101-000000"));
        assert!(mock.contains("/top/@snapshots/@-default-20260102-000000"));
    }

    #[test]
    fn retention_purges_delete_markers() {
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Pre-existing DELETE marker (e.g. left over from a prior restore).
        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));

        let summary = apply_retention(&config, &mock, toplevel).unwrap();
        assert!(
            summary
                .removed
                .contains(&"@-DELETE-20260101-120000".to_string())
        );
        assert!(!mock.contains("/top/@-DELETE-20260101-120000"));
    }

    #[test]
    fn purge_orphaned_sidecars_removes_only_orphans() {
        let config = config_retain_last(&["@"], 5);
        let dir = {
            let p = std::env::temp_dir().join(format!("revenant-cleanup-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            p
        };
        let mock = setup_mock(&config, &dir);
        let snap_dir = dir.join(&config.sys.snapshot_subvol);
        std::fs::create_dir_all(&snap_dir).unwrap();

        // Paired sidecar — a subvolume with matching (strain, id) exists
        // in the mock, so the sidecar must be kept.
        mock.seed_subvolume(snap_dir.join("@-default-20260316-143022"));
        let kept_sidecar = snap_dir.join("default-20260316-143022.meta.toml");
        std::fs::write(&kept_sidecar, "schema_version = 1\ncreated_at = \"2026-04-14T14:05:01+02:00\"\n[trigger]\nkind = \"manual\"\n").unwrap();

        // Orphan — no subvolume with (strain=default, id=20260101-000000).
        let orphan_name = "default-20260101-000000.meta.toml";
        let orphan_path = snap_dir.join(orphan_name);
        std::fs::write(&orphan_path, "schema_version = 1\ncreated_at = \"2026-04-14T14:05:01+02:00\"\n[trigger]\nkind = \"manual\"\n").unwrap();

        let removed = purge_orphaned_sidecars(&config, &mock, &dir).unwrap();
        assert_eq!(removed, vec![orphan_name.to_string()]);
        assert!(!orphan_path.exists());
        assert!(kept_sidecar.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn purge_orphaned_sidecars_snap_dir_missing_is_noop() {
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        let removed = purge_orphaned_sidecars(&config, &mock, toplevel).unwrap();
        assert!(removed.is_empty());
    }

    // ----- plan_retention -----

    #[test]
    fn plan_reports_keep_and_delete_per_strain() {
        // default retains last=2, periodic retains last=1. Seed three
        // snapshots in each; the plan must flag the oldest-in-default and
        // the two oldest-in-periodic as Delete, and preserve newest-first
        // ordering within each strain.
        let config = config_two_strains_retain(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        for ts in &["20260101-000000", "20260102-000000", "20260103-000000"] {
            seed_snapshot_for(&mock, &config, toplevel, "default", ts);
            seed_snapshot_for(&mock, &config, toplevel, "periodic", ts);
        }

        let plan = plan_retention(&config, &mock, toplevel).unwrap();
        // Plan must not touch disk.
        assert!(mock.contains("/top/@snapshots/@-default-20260101-000000"));
        assert!(mock.contains("/top/@snapshots/@-periodic-20260101-000000"));

        // Strain ordering is stable (alphabetical).
        let names: Vec<&str> = plan.strains.iter().map(|s| s.strain.as_str()).collect();
        assert_eq!(names, vec!["default", "periodic"]);

        // default: newest two Kept (last), oldest Deleted.
        let default = &plan.strains[0];
        assert_eq!(default.entries.len(), 3);
        assert_eq!(default.entries[0].id.as_str(), "20260103-000000");
        assert_eq!(default.entries[2].id.as_str(), "20260101-000000");
        assert!(matches!(
            default.entries[0].action,
            PlanAction::Keep { ref reasons } if reasons == &vec![KeepReason::Last]
        ));
        assert!(matches!(default.entries[2].action, PlanAction::Delete));

        // periodic: only newest Kept.
        let periodic = &plan.strains[1];
        assert!(matches!(
            periodic.entries[0].action,
            PlanAction::Keep { .. }
        ));
        assert!(matches!(periodic.entries[1].action, PlanAction::Delete));
        assert!(matches!(periodic.entries[2].action, PlanAction::Delete));

        // No DELETE markers in this scenario.
        assert!(plan.delete_markers.is_empty());
    }

    #[test]
    fn plan_lists_empty_strains() {
        // A configured strain with no snapshots on disk should still appear
        // in the plan, with an empty entries list — the renderer prints
        // "(no snapshots)" for it.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        let plan = plan_retention(&config, &mock, toplevel).unwrap();
        assert_eq!(plan.strains.len(), 1);
        assert_eq!(plan.strains[0].strain, "default");
        assert!(plan.strains[0].entries.is_empty());
    }

    #[test]
    fn plan_reports_delete_markers() {
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));
        // A foreign DELETE-ish name on an unknown base must NOT be listed.
        mock.seed_subvolume(toplevel.join("@home-DELETE-20260101-120000"));

        let plan = plan_retention(&config, &mock, toplevel).unwrap();
        assert_eq!(plan.delete_markers.len(), 1);
        let marker = &plan.delete_markers[0];
        assert_eq!(marker.name, "@-DELETE-20260101-120000");
        assert_eq!(marker.base_subvol, "@");
        assert_eq!(marker.id.as_str(), "20260101-120000");
        // Dry run: marker still present on disk.
        assert!(mock.contains("/top/@-DELETE-20260101-120000"));
    }

    #[test]
    fn plan_drops_unparsable_delete_markers() {
        // A `<base>-DELETE-<garbage>` name where <garbage> is not a
        // valid snapshot ID must be excluded from the dry-run report —
        // we don't have a trustworthy timestamp for JSON consumers.
        // (purge_delete_pending_all would still remove it, since it
        // only matches the `<base>-DELETE-` prefix.)
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume(toplevel.join("@-DELETE-not-a-timestamp"));
        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));

        let plan = plan_retention(&config, &mock, toplevel).unwrap();
        // Only the well-formed one shows up.
        assert_eq!(plan.delete_markers.len(), 1);
        assert_eq!(plan.delete_markers[0].name, "@-DELETE-20260101-120000");
    }

    // ----- purge_delete_pending_all -----

    #[test]
    fn purge_skips_unrelated_subvols() {
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Three things in toplevel that look superficially similar:
        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000")); // real marker
        mock.seed_subvolume(toplevel.join("@home-DELETE-20260101-120000")); // unknown base, ignored
        mock.seed_subvolume(toplevel.join("@-default-20260102-120000")); // not a DELETE marker

        let removed = purge_delete_pending_all(&config, &mock, toplevel).unwrap();
        assert_eq!(removed, vec!["@-DELETE-20260101-120000".to_string()]);
        assert!(!mock.contains("/top/@-DELETE-20260101-120000"));
        // Other entries untouched
        assert!(mock.contains("/top/@home-DELETE-20260101-120000"));
        assert!(mock.contains("/top/@-default-20260102-120000"));
    }

    #[test]
    fn purge_recurses_into_delete_marker_with_nested_subvols() {
        // Regression: a restore renames the live `@` to `@-DELETE-{ts}`,
        // which carries any nested subvolumes (e.g. var/lib/portables on
        // Arch) along with it. btrfs SNAP_DESTROY refuses to delete a
        // subvolume that still contains nested ones (ENOTEMPTY), so the
        // marker would otherwise stay around forever.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // The DELETE marker plus two nested subvols inside it (the
        // mock's delete_subvolume mirrors the real ENOTEMPTY behaviour,
        // so this test would fail without the recursive cleanup).
        let marker = toplevel.join("@-DELETE-20260101-120000");
        mock.seed_subvolume(marker.clone());
        mock.seed_subvolume(marker.join("var/lib/portables"));
        mock.seed_subvolume(marker.join("var/lib/machines"));

        let removed = purge_delete_pending_all(&config, &mock, toplevel).unwrap();
        assert_eq!(removed, vec!["@-DELETE-20260101-120000".to_string()]);
        assert!(!mock.contains(&marker));
        assert!(!mock.contains(marker.join("var/lib/portables")));
        assert!(!mock.contains(marker.join("var/lib/machines")));
    }

    #[test]
    fn purge_handles_multiple_markers() {
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));
        mock.seed_subvolume(toplevel.join("@-DELETE-20260102-120000"));
        mock.seed_subvolume(toplevel.join("@-DELETE-20260103-120000"));

        let removed = purge_delete_pending_all(&config, &mock, toplevel).unwrap();
        assert_eq!(removed.len(), 3);
        for ts in &["20260101-120000", "20260102-120000", "20260103-120000"] {
            assert!(!mock.contains(format!("/top/@-DELETE-{ts}")));
        }
    }

    // ----- recover_orphaned_nested_subvols -----

    #[test]
    fn recover_moves_nested_back_into_live_subvol() {
        // Simulates a crash between the rename of the live @ and the
        // re-attach loop in restore_snapshot: there's a fresh @ (the
        // restored one) plus a @-DELETE-{ts} carrying a nested subvol
        // that never got moved back.  The recovery hook should put it
        // back into @ at the same relative path.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        let marker = toplevel.join("@-DELETE-20260101-120000");
        mock.seed_subvolume(marker.clone());
        mock.seed_subvolume(marker.join("var/lib/portables"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        assert_eq!(recovered, 1);
        assert!(mock.contains("/top/@/var/lib/portables"));
        assert!(!mock.contains(marker.join("var/lib/portables")));
        // Marker itself stays — purge handles its removal once it's
        // empty of nested subvols.
        assert!(mock.contains(&marker));
    }

    #[test]
    fn recover_skips_marker_without_nested() {
        // The normal post-restore state: the marker is empty of nested
        // subvols.  Recovery should be a no-op.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        assert_eq!(recovered, 0);
    }

    #[test]
    fn recover_warns_when_live_subvol_missing() {
        // Pathological state: a marker carries nested subvols but the
        // live @ doesn't exist (e.g. crash between rename @ and the
        // create_writable_snapshot step).  Recovery should leave the
        // marker alone — moving the nested subvol into a non-existent
        // path would be wrong, and the system can't have booted
        // anyway.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        // Note: deliberately NOT seeding /top/@.
        mock.seed_subvolume(toplevel.join(&config.sys.snapshot_subvol));
        let marker = toplevel.join("@-DELETE-20260101-120000");
        mock.seed_subvolume(marker.clone());
        mock.seed_subvolume(marker.join("var/lib/portables"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        assert_eq!(recovered, 0);
        // Nothing was moved.
        assert!(mock.contains(marker.join("var/lib/portables")));
        assert!(!mock.contains("/top/@/var/lib/portables"));
    }

    #[test]
    fn recover_skips_destination_collision() {
        // The user installed something between the crashed restore and
        // the recovery run: the live @ now has its OWN subvol at
        // var/lib/portables.  We must not overwrite that — the orphan
        // gets left in the marker for manual review.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Live @ already has a subvol at the colliding path.
        mock.seed_subvolume("/top/@/var/lib/portables");

        let marker = toplevel.join("@-DELETE-20260101-120000");
        mock.seed_subvolume(marker.clone());
        mock.seed_subvolume(marker.join("var/lib/portables"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        assert_eq!(recovered, 0);
        // Both copies still present — the live one untouched, the
        // orphan still in the marker.
        assert!(mock.contains("/top/@/var/lib/portables"));
        assert!(mock.contains(marker.join("var/lib/portables")));
    }

    #[test]
    fn recover_handles_multiple_markers_and_nested() {
        // Two DELETE markers, each with multiple nested subvols.  All
        // should land in the live @ at their original relative paths.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        let m1 = toplevel.join("@-DELETE-20260101-120000");
        mock.seed_subvolume(m1.clone());
        mock.seed_subvolume(m1.join("var/lib/portables"));
        mock.seed_subvolume(m1.join("var/lib/machines"));

        let m2 = toplevel.join("@-DELETE-20260102-130000");
        mock.seed_subvolume(m2.clone());
        mock.seed_subvolume(m2.join("var/cache/build"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        // 3 nested rescued in total.  Note that having two markers
        // each carrying overlapping paths would surface as a
        // destination collision on the second one — that's tested
        // separately.
        assert_eq!(recovered, 3);
        assert!(mock.contains("/top/@/var/lib/portables"));
        assert!(mock.contains("/top/@/var/lib/machines"));
        assert!(mock.contains("/top/@/var/cache/build"));
    }

    #[test]
    fn recover_ignores_markers_for_unknown_bases() {
        // Unrelated `<unknown>-DELETE-<ts>` entries (e.g. from a
        // different tool, or a removed strain subvol) must not be
        // touched even if they happen to contain nested subvols.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        let foreign = toplevel.join("@home-DELETE-20260101-120000");
        mock.seed_subvolume(foreign.clone());
        mock.seed_subvolume(foreign.join("inner/data"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        assert_eq!(recovered, 0);
        assert!(mock.contains(foreign.join("inner/data")));
    }
}
