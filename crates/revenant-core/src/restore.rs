use std::path::Path;

use crate::backend::{FileSystemBackend, subvol_exists};
use crate::config::{Config, DELETE_STRAIN};
use crate::error::{Result, RevenantError};
use crate::pkgmgr;
use crate::snapshot::{SnapshotId, SnapshotInfo};

/// Orchestrate a full restore from a snapshot.
///
/// Steps:
/// 1. Validate that all expected components exist on disk.
/// 2. For each strain subvol, record any nested subvolumes currently living
///    inside it, rename the live subvolume to `{subvol}-DELETE-{ts}` (the
///    nested subvolumes ride along because their directory entries live in
///    the renamed tree), create a writable snapshot from the chosen
///    snapshot as the new live subvolume, then move each nested subvolume
///    out of the DELETE marker and back into the freshly restored
///    subvolume at the same relative path.  The DELETE marker survives
///    until `revenantctl cleanup` or the next restore explicitly purges
///    it, so it is available as a volatile undo buffer for the previous
///    live state.
/// 3. Sync EFI content back from the boot snapshot to the ESP.
///
/// No automatic pre-restore safety snapshot is created.  The DELETE
/// marker *is* the pre-restore live subvolume (renamed, not copied), and
/// an extra ro-snapshot in the strain only obscured which snapshots came
/// from the configured boot/periodic timers and which were side effects
/// of a restore.  Users who want a retained, strain-integrated safety
/// copy should take a manual `revenantctl snapshot` before invoking
/// restore.
///
/// Re-attaching nested subvolumes preserves their *current* state across
/// the restore — they keep the data they had at the moment of restore, not
/// at the moment the snapshot was taken.  This is intentional: nested
/// subvolumes typically hold runtime state (machinectl images, systemd
/// portables, container layers) that should not get rolled back with `@`.
/// btrfs does not snapshot nested subvolumes by default — the snapshot
/// contains only stub directories at the nested-subvol locations — so a
/// naive restore would otherwise lose that data entirely.
///
/// If the restored snapshot pre-dates the creation of a nested
/// subvolume's parent directory tree (e.g. rolling back to a snapshot
/// taken before `systemd-portables` was installed), the parent path is
/// materialised on the fly via the backend so the re-attach can land.
/// Otherwise the nested data would be stranded in the DELETE marker the
/// first time the user rolls back across the install — and would be
/// silently lost the next time they tried to roll forward again.
pub fn restore_snapshot(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    snapshot: &SnapshotInfo,
) -> Result<()> {
    let id = &snapshot.id;
    let strain = &snapshot.strain;
    let strain_config = config.strain(strain)?;

    let snap_dir = toplevel.join(&config.sys.snapshot_subvol);
    tracing::info!("restoring snapshot {id} (strain: {strain})");

    // 1. Validate: all snapshot components must be present
    let mut missing = Vec::new();
    for subvol in &strain_config.subvolumes {
        let snap_path = snap_dir.join(id.snapshot_name(subvol, strain));
        if !subvol_exists(backend, &snap_path) {
            missing.push(subvol.clone());
        }
    }
    if strain_config.efi && config.sys.efi.enabled {
        let staging = &config.sys.efi.staging_subvol;
        let snap_path = snap_dir.join(id.snapshot_name(staging, strain));
        if !subvol_exists(backend, &snap_path) {
            missing.push(staging.clone());
        }
    }
    if !missing.is_empty() {
        return Err(RevenantError::IncompleteSnapshot {
            id: id.to_string(),
            missing,
        });
    }

    // Fresh timestamp used as the suffix on the DELETE markers so every
    // subvol touched by this restore ends up under the same marker id.
    let ts = SnapshotId::now();

    // 2. For each subvol: collect nested → rename → restore → reattach.
    for subvol in &strain_config.subvolumes {
        let current = toplevel.join(subvol);
        let delete_name = format!("{subvol}-{DELETE_STRAIN}-{ts}");
        let delete_path = toplevel.join(&delete_name);

        // (a) Record nested subvolumes BEFORE the rename, so we know which
        //     paths to re-attach afterwards.  Direct nested only — anything
        //     deeper rides along inside its parent and reappears under it
        //     when the parent is reattached.
        let nested = backend.find_nested_subvolumes(&current)?;
        if !nested.is_empty() {
            tracing::info!(
                "found {} nested subvolume(s) in {} that will be carried across restore",
                nested.len(),
                current.display()
            );
        }

        // (b) Rename current → DELETE marker.  Nested subvols come along
        //     because their directory entries live in the renamed tree.
        tracing::info!("marking current {subvol} for deletion as {delete_name}");
        backend.rename_subvolume(&current, &delete_path)?;

        // (c) Restore from the chosen snapshot.
        let snap_path = snap_dir.join(id.snapshot_name(subvol, strain));
        tracing::info!("restoring {subvol} from snapshot {id}");
        backend.create_writable_snapshot(&snap_path, &current)?;

        // (d) Re-attach nested subvolumes from the DELETE marker into the
        //     freshly restored subvolume.  The snapshot itself contains
        //     only stub directories at the nested-subvol locations (btrfs
        //     does not snapshot nested subvols by default), so the rename
        //     replaces those empty stubs with the live nested subvolume.
        for nested_path in &nested {
            let rel = nested_path.strip_prefix(&current).map_err(|_| {
                RevenantError::Other(format!(
                    "nested subvol {} unexpectedly outside {}",
                    nested_path.display(),
                    current.display(),
                ))
            })?;
            let from = delete_path.join(rel);
            let to = current.join(rel);

            // Materialise the parent path in case the restored snapshot
            // pre-dates the directory tree that leads up to the nested
            // subvolume's location.  No-op when the parent already
            // exists (the common case).
            if let Some(parent) = to.parent() {
                backend.create_dir_all(parent)?;
            }

            tracing::info!(
                "re-attaching nested subvolume {} → {}",
                from.display(),
                to.display()
            );
            backend.rename_subvolume(&from, &to)?;
        }
    }

    // 2b. Strip package-manager runtime state that is only meaningful
    //     for the *live* system. PreTransaction hooks (notably pacman's)
    //     fire with the PM's lock file already held, so the resulting
    //     snapshot always carries a stale lock. Delivering that lock
    //     back to a freshly restored tree makes the next package
    //     operation fail with a bewildering "unable to lock database"
    //     error. Do this unconditionally for every known PM: missing
    //     files are a no-op, and a system that has never seen pacman
    //     simply has nothing to strip.
    let rootfs = toplevel.join(&config.sys.rootfs_subvol);
    cleanup_stale_runtime_files(&rootfs);

    // 3. Restore EFI
    if snapshot.efi_synced && config.sys.efi.enabled {
        let staging = &config.sys.efi.staging_subvol;
        let snap = snap_dir.join(id.snapshot_name(staging, strain));
        tracing::info!("restoring EFI from {}", snap.display());
        crate::efi::sync_to_staging(&snap, &config.sys.efi.mount_point)?;
    }

    tracing::info!("restore complete — please reboot to apply changes");
    Ok(())
}

/// Remove stale package-manager runtime files from a freshly restored
/// rootfs subvolume.
///
/// Consults [`pkgmgr::all_package_managers`] unconditionally: the set
/// of known PMs is tiny and each one's claim about what is "stale" is
/// strictly scoped to its own runtime footprint, so running every PM's
/// cleanup on every restore is safe — a file that does not exist is a
/// no-op.
///
/// Errors are never propagated. A failure to strip one of these files
/// is strictly less destructive than aborting the restore (the tree is
/// already renamed into place, the user is committed to rebooting into
/// it), and the failure mode we are actually trying to prevent — pacman
/// refusing to run post-restore because of a baked-in `db.lck` — surfaces
/// loudly the next time the user runs `pacman`. Non-`NotFound` errors are
/// logged as warnings so they show up in diagnostics without turning a
/// successful restore into a failed one.
fn cleanup_stale_runtime_files(rootfs: &Path) {
    for pm in pkgmgr::all_package_managers() {
        for rel in pm.stale_runtime_files() {
            let target = rootfs.join(rel);
            match std::fs::remove_file(&target) {
                Ok(()) => {
                    tracing::info!(
                        "stripped stale {} runtime file: {}",
                        pm.name(),
                        target.display()
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Expected: the snapshot never contained this
                    // file, or we already cleaned it on a previous
                    // restore. Nothing to do.
                }
                Err(e) => {
                    tracing::warn!(
                        "failed to strip stale {} runtime file {}: {}",
                        pm.name(),
                        target.display(),
                        e
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::backend::mock::MockBackend;
    use crate::metadata::Trigger;
    use crate::snapshot::{SnapshotId, create_snapshot, discover_snapshots};

    /// Build a no-EFI Config with one strain `default` covering the given subvols.
    fn config_no_efi(subvols: &[&str]) -> Config {
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
"#
        );
        toml.parse().unwrap()
    }

    /// Seed the mock with the configured base subvolumes plus the snapshot
    /// subvolume itself.
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

    /// Pre-seed a complete snapshot for a given strain/id covering all configured subvols.
    fn seed_snapshot(mock: &MockBackend, config: &Config, toplevel: &Path, strain: &str, id: &str) {
        let snap_dir = toplevel.join(&config.sys.snapshot_subvol);
        let sc = config.strain.get(strain).unwrap();
        for sv in &sc.subvolumes {
            mock.seed_subvolume(snap_dir.join(format!("{sv}-{strain}-{id}")));
        }
    }

    fn snap_info(id: &str, strain: &str, subvols: &[&str]) -> SnapshotInfo {
        SnapshotInfo {
            id: SnapshotId::from_string(id).unwrap(),
            strain: strain.to_string(),
            subvolumes: subvols.iter().map(|s| (*s).to_string()).collect(),
            efi_synced: false,
            metadata: None,
        }
    }

    #[test]
    fn happy_path_single_subvol() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        seed_snapshot(&mock, &config, toplevel, "default", "20260316-143022");

        let snap = snap_info("20260316-143022", "default", &["@"]);
        restore_snapshot(&config, &mock, toplevel, &snap).unwrap();

        // The live @ subvolume still exists (recreated as writable snapshot).
        assert!(mock.contains("/top/@"));
    }

    #[test]
    fn does_not_create_auto_safety_snapshot() {
        // The old behaviour was to create a pre-restore snapshot under
        // the same strain as the one being restored.  That polluted the
        // strain's snapshot list with something that wasn't a regular
        // timer-driven snapshot and confused users about which entries
        // were boot-unit snapshots and which were restore side effects.
        // The live pre-restore subvolume is preserved as a DELETE marker
        // (tested separately), which is a more honest representation:
        // the renamed live subvolume, not a second ro copy of it.
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        seed_snapshot(&mock, &config, toplevel, "default", "20260316-143022");

        let snap = snap_info("20260316-143022", "default", &["@"]);
        restore_snapshot(&config, &mock, toplevel, &snap).unwrap();

        // Only the original snapshot exists in the strain — nothing
        // extra was created during the restore.
        let all = discover_snapshots(&config, &mock, toplevel).unwrap();
        let in_default: Vec<_> = all.iter().filter(|s| s.strain == "default").collect();
        assert_eq!(
            in_default.len(),
            1,
            "no auto pre-restore snapshot should be created, got: {in_default:?}"
        );
        assert_eq!(in_default[0].id.as_str(), "20260316-143022");
    }

    #[test]
    fn delete_marker_survives_subsequent_create_snapshot() {
        // After a restore, the DELETE marker must survive routine
        // snapshot creation (boot unit, periodic timer, manual
        // `revenantctl snapshot`) — it is the volatile undo buffer
        // for the previous live state, purged only by
        // `revenantctl cleanup` / the next restore.
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        seed_snapshot(&mock, &config, toplevel, "default", "20260316-143022");

        let snap = snap_info("20260316-143022", "default", &["@"]);
        restore_snapshot(&config, &mock, toplevel, &snap).unwrap();

        // Grab the marker that the restore produced.
        let marker_before = mock
            .all_paths()
            .into_iter()
            .find(|p| {
                p.parent() == Some(toplevel)
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("@-DELETE-"))
            })
            .expect("expected DELETE marker after restore");

        // Simulate a post-boot snapshot run.
        create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            None,
            Trigger::default(),
        )
        .unwrap();

        // Marker still present at the same path.
        assert!(
            mock.contains(&marker_before),
            "DELETE marker must not be purged by create_snapshot"
        );
    }

    #[test]
    fn renames_current_to_delete_marker() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        seed_snapshot(&mock, &config, toplevel, "default", "20260316-143022");

        let snap = snap_info("20260316-143022", "default", &["@"]);
        restore_snapshot(&config, &mock, toplevel, &snap).unwrap();

        // A `@-DELETE-{ts}` marker should now sit in toplevel.
        let delete_marker = mock.all_paths().into_iter().find(|p| {
            p.parent() == Some(toplevel)
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("@-DELETE-"))
        });
        assert!(
            delete_marker.is_some(),
            "expected @-DELETE-* marker in toplevel"
        );
    }

    #[test]
    fn rejects_incomplete_snapshot() {
        let config = config_no_efi(&["@", "@home"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        // Seed only @ snapshot, not @home — so the snapshot is incomplete
        // for the strain.
        let snap_dir = toplevel.join(&config.sys.snapshot_subvol);
        mock.seed_subvolume(snap_dir.join("@-default-20260316-143022"));

        let snap = snap_info("20260316-143022", "default", &["@", "@home"]);
        let err = restore_snapshot(&config, &mock, toplevel, &snap).unwrap_err();
        match err {
            RevenantError::IncompleteSnapshot { id, missing } => {
                assert_eq!(id, "20260316-143022");
                assert_eq!(missing, vec!["@home".to_string()]);
            }
            other => panic!("expected IncompleteSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn nested_subvols_are_reattached_after_restore() {
        // Regression: a stock Arch system has nested subvolumes inside @
        // (var/lib/portables, var/lib/machines).  The snapshot does not
        // contain those nested subvols (btrfs replaces them with stub
        // directories), so a naive restore would lose the data.  We
        // expect the live nested subvols to be carried across the
        // restore at their *current* state — see the module docs.
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Pre-existing nested subvols inside the live @.
        mock.seed_subvolume("/top/@/var/lib/portables");
        mock.seed_subvolume("/top/@/var/lib/machines");

        // The snapshot itself contains only the @ entry — no nested
        // children, mirroring real btrfs snapshot semantics.
        seed_snapshot(&mock, &config, toplevel, "default", "20260316-143022");

        let snap = snap_info("20260316-143022", "default", &["@"]);
        restore_snapshot(&config, &mock, toplevel, &snap).unwrap();

        // After restore: nested subvols are at their original paths
        // under the freshly restored @.
        assert!(
            mock.contains("/top/@/var/lib/portables"),
            "expected nested portables subvol to be re-attached"
        );
        assert!(
            mock.contains("/top/@/var/lib/machines"),
            "expected nested machines subvol to be re-attached"
        );

        // The DELETE marker no longer carries the nested subvols — they
        // were moved out, not copied.
        let delete_marker = mock
            .all_paths()
            .into_iter()
            .find(|p| {
                p.parent() == Some(toplevel)
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("@-DELETE-"))
            })
            .expect("expected @-DELETE-* marker after restore");
        assert!(
            !mock.contains(delete_marker.join("var/lib/portables")),
            "nested portables should have been moved out of the DELETE marker"
        );
        assert!(
            !mock.contains(delete_marker.join("var/lib/machines")),
            "nested machines should have been moved out of the DELETE marker"
        );
    }

    #[test]
    fn nested_subvols_per_strain_subvol() {
        // Re-attach must run independently for each strain subvol —
        // a nested subvol inside @home should not leak into @ and vice
        // versa.
        let config = config_no_efi(&["@", "@home"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume("/top/@/var/lib/portables");
        mock.seed_subvolume("/top/@home/.cache/build");

        seed_snapshot(&mock, &config, toplevel, "default", "20260316-143022");

        let snap = snap_info("20260316-143022", "default", &["@", "@home"]);
        restore_snapshot(&config, &mock, toplevel, &snap).unwrap();

        assert!(mock.contains("/top/@/var/lib/portables"));
        assert!(mock.contains("/top/@home/.cache/build"));
        // Cross-contamination check: portables should not have ended up
        // under @home, build should not have ended up under @.
        assert!(!mock.contains("/top/@home/var/lib/portables"));
        assert!(!mock.contains("/top/@/.cache/build"));
    }

    #[test]
    fn nested_subvol_parent_path_is_materialised() {
        // The crucial cross-restore-boundary case: roll back to a
        // snapshot that pre-dates the creation of the nested
        // subvolume's parent directories.  Without `create_dir_all`,
        // the rename of the nested subvolume into the freshly restored
        // @ would have nowhere to land — the parent path simply
        // doesn't exist in the restored tree — and the data would be
        // stranded in the DELETE marker.  Verify that the orchestration
        // asks the backend to materialise the parent path before the
        // re-attach, and that the rename lands.
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Live @ has a nested subvol whose parent path the snapshot
        // (taken before the nested subvol was ever installed) does
        // not contain.  The mock has no notion of plain directories,
        // so this is a slight abstraction — the test focus is the
        // create_dir_all call recorded by the mock.
        mock.seed_subvolume("/top/@/var/lib/portables");

        seed_snapshot(&mock, &config, toplevel, "default", "20260316-143022");

        let snap = snap_info("20260316-143022", "default", &["@"]);
        restore_snapshot(&config, &mock, toplevel, &snap).unwrap();

        // The nested subvol must have ended up at its original path.
        assert!(mock.contains("/top/@/var/lib/portables"));

        // And the orchestration must have asked the backend to
        // materialise its parent path (`@/var/lib`) before the rename.
        // Without this call the rename would fail on real btrfs when
        // the snapshot pre-dates `var/lib`.
        let created = mock.created_dirs();
        assert!(
            created.contains(&PathBuf::from("/top/@/var/lib")),
            "expected create_dir_all('/top/@/var/lib'), got: {created:?}"
        );
    }

    #[test]
    fn deeply_nested_subvols_ride_along() {
        // find_nested_subvolumes only returns *direct* children, but
        // anything deeper rides along inside its parent automatically:
        // when we reattach @/var/lib/portables we are renaming the
        // entire subtree, so a sub-nested subvol inside it comes along
        // for free.  Verify that.
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume("/top/@/var/lib/portables");
        mock.seed_subvolume("/top/@/var/lib/portables/inner");

        seed_snapshot(&mock, &config, toplevel, "default", "20260316-143022");

        let snap = snap_info("20260316-143022", "default", &["@"]);
        restore_snapshot(&config, &mock, toplevel, &snap).unwrap();

        assert!(mock.contains("/top/@/var/lib/portables"));
        assert!(mock.contains("/top/@/var/lib/portables/inner"));
    }

    #[test]
    fn multi_subvol_restore() {
        let config = config_no_efi(&["@", "@home"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        seed_snapshot(&mock, &config, toplevel, "default", "20260316-143022");

        let snap = snap_info("20260316-143022", "default", &["@", "@home"]);
        restore_snapshot(&config, &mock, toplevel, &snap).unwrap();

        // Both live subvolumes still present after restore.
        assert!(mock.contains("/top/@"));
        assert!(mock.contains("/top/@home"));
        // And both have a DELETE marker.
        let markers: Vec<_> = mock
            .all_paths()
            .into_iter()
            .filter(|p| {
                p.parent() == Some(toplevel)
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.contains("-DELETE-"))
            })
            .collect();
        assert_eq!(
            markers.len(),
            2,
            "expected one DELETE marker per subvol, got: {markers:?}"
        );
    }
}
