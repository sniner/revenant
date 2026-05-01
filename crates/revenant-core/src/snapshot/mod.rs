//! Snapshot identity, discovery, and lifecycle.
//!
//! Split into submodules by concern:
//!
//! * [`id`] — `SnapshotId` (timestamp-based identifier) and the
//!   `subvol-strain-id` name parser/builder pair.
//! * [`target`] — `SnapshotTarget`, the user-facing addressing form
//!   (`strain@id` / `id` / `strain@`).
//! * [`info`] — `SnapshotInfo` plus the on-disk path helpers shared
//!   between discovery and operations.
//! * [`discovery`] — `discover_snapshots`, `find_snapshot`,
//!   `resolve_live_parent`, and `LiveParentRef`.
//! * [`operations`] — `create_snapshot`, `delete_snapshot`,
//!   `update_snapshot_metadata`, and the bulk variants.
//!
//! Public items are re-exported here so every external consumer can
//! continue to import them as `revenant_core::snapshot::*`.

mod discovery;
mod id;
mod info;
mod operations;
mod target;

pub use discovery::{LiveParentRef, discover_snapshots, find_snapshot, resolve_live_parent};
pub use id::{SnapshotId, parse_snapshot_subvol_name};
pub use info::{SnapshotInfo, qualified};
pub use operations::{
    BulkDeleteOutcome, MetadataPatch, create_snapshot, delete_all_strain, delete_snapshot,
    update_snapshot_metadata,
};
pub use target::SnapshotTarget;

// Private items reached only by the in-module test block; brought into
// scope here so `super::*` in tests stays straightforward.
#[cfg(test)]
use id::ID_LEN_CURRENT;
#[cfg(test)]
use info::{sidecar_path_for_snapshot, snapshot_dir};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FileSystemBackend;
    use crate::backend::mock::MockBackend;
    use crate::config::Config;
    use crate::error::RevenantError;
    use crate::metadata::{self, SnapshotMetadata, TriggerKind};
    use std::path::Path;

    /// Build a minimal Config with a single strain `default` covering the
    /// given subvolumes. Snapshot subvol is `@snapshots`, no EFI.
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

    /// Build a Config with two strains over the same subvol set.
    fn config_two_strains(subvols: &[&str]) -> Config {
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

[strain.periodic]
subvolumes = [{subvol_list}]
"#
        );
        toml.parse().unwrap()
    }

    /// Set up a mock backend pre-populated with the configured base
    /// subvolumes and the snapshot directory subvolume.
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

    // ----- discover_snapshots -----

    #[test]
    fn discover_empty_when_no_snapshots() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        let snaps = discover_snapshots(&config, &mock, toplevel).unwrap();
        assert!(snaps.is_empty());
    }

    #[test]
    fn discover_returns_empty_if_snap_dir_missing() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        // Note: snapshot subvol intentionally NOT seeded
        let snaps = discover_snapshots(&config, &mock, toplevel).unwrap();
        assert!(snaps.is_empty());
    }

    #[test]
    fn discover_finds_single_strain_snapshot() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-143022"));

        let snaps = discover_snapshots(&config, &mock, toplevel).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].id.as_str(), "20260316-143022");
        assert_eq!(snaps[0].strain, "default");
        assert_eq!(snaps[0].subvolumes, vec!["@".to_string()]);
        assert!(!snaps[0].efi_synced);
    }

    #[test]
    fn discover_groups_multi_subvol_snapshot() {
        let config = config_no_efi(&["@", "@home"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-143022"));
        mock.seed_subvolume(toplevel.join("@snapshots/@home-default-20260316-143022"));

        let snaps = discover_snapshots(&config, &mock, toplevel).unwrap();
        assert_eq!(
            snaps.len(),
            1,
            "the two subvols should be grouped into one snapshot"
        );
        let mut subs = snaps[0].subvolumes.clone();
        subs.sort();
        assert_eq!(subs, vec!["@".to_string(), "@home".to_string()]);
    }

    #[test]
    fn discover_separates_strains() {
        let config = config_two_strains(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-143022"));
        mock.seed_subvolume(toplevel.join("@snapshots/@-periodic-20260316-150000"));

        let snaps = discover_snapshots(&config, &mock, toplevel).unwrap();
        assert_eq!(snaps.len(), 2);
        let strains: Vec<&str> = snaps.iter().map(|s| s.strain.as_str()).collect();
        assert!(strains.contains(&"default"));
        assert!(strains.contains(&"periodic"));
    }

    #[test]
    fn discover_ignores_unrelated_entries() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        // Looks like a snapshot but for an unconfigured strain
        mock.seed_subvolume(toplevel.join("@snapshots/@-other-20260316-143022"));
        // Not a snapshot at all
        mock.seed_subvolume(toplevel.join("@snapshots/random-thing"));
        // Configured strain but unconfigured subvol
        mock.seed_subvolume(toplevel.join("@snapshots/@home-default-20260316-143022"));

        let snaps = discover_snapshots(&config, &mock, toplevel).unwrap();
        assert!(
            snaps.is_empty(),
            "none of the entries match a configured (subvol, strain) pair"
        );
    }

    #[test]
    fn discover_sorts_chronologically() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-150000"));
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-120000"));
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-130000"));

        let snaps = discover_snapshots(&config, &mock, toplevel).unwrap();
        let ids: Vec<&str> = snaps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["20260316-120000", "20260316-130000", "20260316-150000"]
        );
    }

    // ----- create_snapshot -----

    #[test]
    fn create_snapshot_single_subvol() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));

        let info = create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap();
        assert_eq!(info.strain, "default");
        assert_eq!(info.subvolumes, vec!["@".to_string()]);
        assert!(!info.efi_synced);

        // Snapshot subvol was created on demand, plus the new readonly snapshot
        let expected = toplevel
            .join("@snapshots")
            .join(info.id.snapshot_name("@", "default"));
        assert!(mock.contains(&expected));
        let snap_info = mock.subvolume_info(&expected).unwrap();
        assert!(snap_info.readonly);
    }

    #[test]
    fn create_snapshot_creates_snap_dir_if_missing() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));
        // Note: @snapshots NOT seeded

        let _ = create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap();
        assert!(mock.contains(toplevel.join("@snapshots")));
    }

    #[test]
    fn create_snapshot_multi_subvol() {
        let config = config_no_efi(&["@", "@home"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));
        mock.seed_subvolume(toplevel.join("@home"));

        let info = create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap();
        let mut subs = info.subvolumes.clone();
        subs.sort();
        assert_eq!(subs, vec!["@".to_string(), "@home".to_string()]);

        let snap_dir = toplevel.join("@snapshots");
        assert!(mock.contains(snap_dir.join(info.id.snapshot_name("@", "default"))));
        assert!(mock.contains(snap_dir.join(info.id.snapshot_name("@home", "default"))));
    }

    #[test]
    fn create_snapshot_unknown_strain_fails() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));

        let err = create_snapshot(
            &config,
            &mock,
            toplevel,
            "nonexistent",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("nonexistent"));
    }

    #[test]
    fn create_snapshot_then_discover_round_trip() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));

        let info = create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap();
        let snaps = discover_snapshots(&config, &mock, toplevel).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].id, info.id);
    }

    // ----- delete_snapshot / delete_all_strain -----

    #[test]
    fn delete_snapshot_removes_all_subvols() {
        let config = config_no_efi(&["@", "@home"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));
        mock.seed_subvolume(toplevel.join("@home"));

        let info = create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap();
        let snap_dir = toplevel.join("@snapshots");
        let p_root = snap_dir.join(info.id.snapshot_name("@", "default"));
        let p_home = snap_dir.join(info.id.snapshot_name("@home", "default"));
        assert!(mock.contains(&p_root));
        assert!(mock.contains(&p_home));

        delete_snapshot(&config, &mock, toplevel, &info).unwrap();
        assert!(!mock.contains(&p_root));
        assert!(!mock.contains(&p_home));
    }

    #[test]
    fn delete_snapshot_is_idempotent_on_missing_subvols() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));

        let info = create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap();
        // Delete twice — second call should be a no-op, not an error
        delete_snapshot(&config, &mock, toplevel, &info).unwrap();
        delete_snapshot(&config, &mock, toplevel, &info).unwrap();
    }

    #[test]
    fn delete_all_strain_targets_only_named_strain() {
        // Seeded directly so we can use distinct, controlled IDs without
        // depending on wall-clock resolution between create_snapshot calls.
        let config = config_two_strains(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-100000"));
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-110000"));
        mock.seed_subvolume(toplevel.join("@snapshots/@-periodic-20260316-120000"));

        let removed = delete_all_strain(&config, &mock, toplevel, "default").unwrap();
        assert_eq!(removed.deleted.len(), 2);
        assert!(removed.skipped_protected.is_empty());

        // periodic snapshot survived
        let snaps = discover_snapshots(&config, &mock, toplevel).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].strain, "periodic");
        assert_eq!(snaps[0].id.as_str(), "20260316-120000");
    }

    #[test]
    fn delete_all_strain_empty_when_no_snapshots() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        let removed = delete_all_strain(&config, &mock, toplevel, "default").unwrap();
        assert!(removed.deleted.is_empty());
        assert!(removed.skipped_protected.is_empty());
    }

    // Protected-snapshot tests live in the sidecar-using block below
    // (after `temp_toplevel`), since they need real filesystem I/O for
    // the sidecar.

    // ----- metadata sidecar integration -----
    //
    // These tests use a real temp directory so that sidecar file I/O actually
    // happens (the MockBackend only tracks virtual subvolumes). We pre-create
    // @snapshots/ as a plain directory inside the temp dir and seed the
    // corresponding mock subvolume at the same path — the mock covers the
    // subvol layer, the real filesystem covers the sidecar layer.

    fn temp_toplevel() -> std::path::PathBuf {
        let name = format!("revenant-snapshot-{}", uuid::Uuid::new_v4());
        let p = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn create_snapshot_writes_sidecar() {
        use crate::metadata::{TriggerKind, read};
        let config = config_no_efi(&["@"]);
        let toplevel = temp_toplevel();
        // Pre-create @snapshots as a real directory for the sidecar write.
        std::fs::create_dir_all(toplevel.join("@snapshots")).unwrap();
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));
        mock.seed_subvolume(toplevel.join("@snapshots"));

        let info = create_snapshot(
            &config,
            &mock,
            &toplevel,
            "default",
            TriggerKind::Manual,
            vec!["ci test".into()],
        )
        .unwrap();

        let sidecar = toplevel
            .join("@snapshots")
            .join(format!("default-{}.meta.toml", info.id.as_str()));
        assert!(sidecar.exists(), "sidecar should be written on disk");
        let meta = read(&sidecar).unwrap().unwrap();
        assert_eq!(meta.message, vec!["ci test".to_string()]);
        assert_eq!(meta.trigger, TriggerKind::Manual);
        assert!(info.metadata.is_some());

        std::fs::remove_dir_all(&toplevel).ok();
    }

    #[test]
    fn discover_attaches_metadata_when_sidecar_present() {
        use crate::metadata::{SnapshotMetadata, TriggerKind, write};
        let config = config_no_efi(&["@"]);
        let toplevel = temp_toplevel();
        std::fs::create_dir_all(toplevel.join("@snapshots")).unwrap();
        let mock = setup_mock(&config, &toplevel);
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-143022"));

        let sidecar = toplevel
            .join("@snapshots")
            .join("default-20260316-143022.meta.toml");
        let meta = SnapshotMetadata::new(TriggerKind::Manual, vec!["hello".into()]);
        write(&sidecar, &meta).unwrap();

        let snaps = discover_snapshots(&config, &mock, &toplevel).unwrap();
        assert_eq!(snaps.len(), 1);
        let m = snaps[0].metadata.as_ref().expect("metadata attached");
        assert_eq!(m.message, vec!["hello".to_string()]);
        assert_eq!(m.trigger, TriggerKind::Manual);

        std::fs::remove_dir_all(&toplevel).ok();
    }

    #[test]
    fn discover_tolerates_missing_sidecar() {
        let config = config_no_efi(&["@"]);
        let toplevel = temp_toplevel();
        std::fs::create_dir_all(toplevel.join("@snapshots")).unwrap();
        let mock = setup_mock(&config, &toplevel);
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-143022"));

        let snaps = discover_snapshots(&config, &mock, &toplevel).unwrap();
        assert_eq!(snaps.len(), 1);
        assert!(snaps[0].metadata.is_none());

        std::fs::remove_dir_all(&toplevel).ok();
    }

    #[test]
    fn delete_snapshot_refuses_protected() {
        let config = config_no_efi(&["@"]);
        let toplevel = temp_toplevel();
        std::fs::create_dir_all(toplevel.join("@snapshots")).unwrap();
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));
        mock.seed_subvolume(toplevel.join("@snapshots"));

        let info = create_snapshot(
            &config,
            &mock,
            &toplevel,
            "default",
            TriggerKind::Manual,
            vec![],
        )
        .unwrap();

        // Flip the sidecar to protected.
        let snap_dir = snapshot_dir(&config, &toplevel);
        let sidecar = sidecar_path_for_snapshot(&snap_dir, "default", &info.id);
        let protected_meta =
            SnapshotMetadata::new(TriggerKind::Manual, vec![]).with_protected(true);
        metadata::write(&sidecar, &protected_meta).unwrap();

        // Re-discover so the in-memory snapshot carries the flag.
        let snaps = discover_snapshots(&config, &mock, &toplevel).unwrap();
        let protected = snaps.iter().find(|s| s.id == info.id).unwrap();

        let err = delete_snapshot(&config, &mock, &toplevel, protected).unwrap_err();
        match err {
            RevenantError::ProtectedSnapshot { strain, id } => {
                assert_eq!(strain, "default");
                assert_eq!(id, info.id.to_string());
            }
            other => panic!("expected ProtectedSnapshot, got {other:?}"),
        }

        // Subvolumes and sidecar must still be there.
        let snap_path = snap_dir.join(info.id.snapshot_name("@", "default"));
        assert!(mock.contains(&snap_path));
        assert!(metadata::read(&sidecar).unwrap().is_some());

        std::fs::remove_dir_all(&toplevel).ok();
    }

    #[test]
    fn delete_all_strain_skips_protected_and_keeps_going() {
        let config = config_no_efi(&["@"]);
        let toplevel = temp_toplevel();
        std::fs::create_dir_all(toplevel.join("@snapshots")).unwrap();
        let mock = setup_mock(&config, &toplevel);

        // Three default-strain snapshots; middle one protected.
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-100000"));
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-110000"));
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-120000"));

        let snap_dir = snapshot_dir(&config, &toplevel);
        let protected_id = SnapshotId::from_string("20260316-110000").unwrap();
        let sidecar = sidecar_path_for_snapshot(&snap_dir, "default", &protected_id);
        metadata::write(
            &sidecar,
            &SnapshotMetadata::new(TriggerKind::Manual, vec![]).with_protected(true),
        )
        .unwrap();

        let outcome = delete_all_strain(&config, &mock, &toplevel, "default").unwrap();
        assert_eq!(outcome.deleted.len(), 2);
        assert_eq!(
            outcome.skipped_protected,
            vec!["20260316-110000".to_string()]
        );

        // Protected one is the only survivor; sidecar still present.
        let remaining = discover_snapshots(&config, &mock, &toplevel).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, protected_id);
        assert!(metadata::read(&sidecar).unwrap().is_some());

        std::fs::remove_dir_all(&toplevel).ok();
    }

    #[test]
    fn update_snapshot_metadata_applies_patch_in_place() {
        let config = config_no_efi(&["@"]);
        let toplevel = temp_toplevel();
        std::fs::create_dir_all(toplevel.join("@snapshots")).unwrap();
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));
        mock.seed_subvolume(toplevel.join("@snapshots"));

        let info = create_snapshot(
            &config,
            &mock,
            &toplevel,
            "default",
            TriggerKind::Manual,
            vec!["original".into()],
        )
        .unwrap();

        let patch = MetadataPatch {
            protected: Some(true),
            message: Some(vec!["edited".into(), "second line".into()]),
        };
        let updated = update_snapshot_metadata(&config, &toplevel, &info, &patch).unwrap();
        assert!(updated.protected);
        assert_eq!(updated.message, vec!["edited", "second line"]);

        // Re-read straight from disk to confirm it persisted.
        let snap_dir = snapshot_dir(&config, &toplevel);
        let sidecar = sidecar_path_for_snapshot(&snap_dir, "default", &info.id);
        let on_disk = metadata::read(&sidecar).unwrap().unwrap();
        assert!(on_disk.protected);
        assert_eq!(on_disk.message, vec!["edited", "second line"]);
        assert_eq!(on_disk.trigger, TriggerKind::Manual); // untouched

        std::fs::remove_dir_all(&toplevel).ok();
    }

    #[test]
    fn update_snapshot_metadata_errors_when_sidecar_missing() {
        let config = config_no_efi(&["@"]);
        let toplevel = temp_toplevel();
        std::fs::create_dir_all(toplevel.join("@snapshots")).unwrap();
        let mock = setup_mock(&config, &toplevel);
        // Snapshot subvol exists, but no sidecar was ever written.
        mock.seed_subvolume(toplevel.join("@snapshots/@-default-20260316-100000"));

        let snaps = discover_snapshots(&config, &mock, &toplevel).unwrap();
        let info = snaps.into_iter().next().unwrap();
        assert!(info.metadata.is_none());

        let patch = MetadataPatch {
            protected: Some(true),
            message: None,
        };
        let err = update_snapshot_metadata(&config, &toplevel, &info, &patch).unwrap_err();
        assert!(matches!(err, RevenantError::Other(_)));

        std::fs::remove_dir_all(&toplevel).ok();
    }

    #[test]
    fn sidecar_naming_is_stable_under_subvolume_reorder() {
        // Regression: the sidecar used to be anchored on
        // config.strain[…].subvolumes.first(). Reordering the list then
        // silently orphaned existing metadata. The new naming scheme is
        // (strain, id) only, so the sidecar must remain discoverable.
        use crate::metadata::TriggerKind;

        let config_a = config_no_efi(&["@", "@home"]);
        let config_b = config_no_efi(&["@home", "@"]); // same strain, reversed
        let toplevel = temp_toplevel();
        std::fs::create_dir_all(toplevel.join("@snapshots")).unwrap();
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));
        mock.seed_subvolume(toplevel.join("@home"));
        mock.seed_subvolume(toplevel.join("@snapshots"));

        let info = create_snapshot(
            &config_a,
            &mock,
            &toplevel,
            "default",
            TriggerKind::Manual,
            vec!["anchor-test".into()],
        )
        .unwrap();

        let snaps = discover_snapshots(&config_b, &mock, &toplevel).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].id, info.id);
        let m = snaps[0]
            .metadata
            .as_ref()
            .expect("metadata must survive subvolume reorder");
        assert_eq!(m.message, vec!["anchor-test".to_string()]);
        assert_eq!(m.trigger, TriggerKind::Manual);

        std::fs::remove_dir_all(&toplevel).ok();
    }

    #[test]
    fn delete_snapshot_removes_sidecar() {
        let config = config_no_efi(&["@"]);
        let toplevel = temp_toplevel();
        std::fs::create_dir_all(toplevel.join("@snapshots")).unwrap();
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));
        mock.seed_subvolume(toplevel.join("@snapshots"));

        let info = create_snapshot(
            &config,
            &mock,
            &toplevel,
            "default",
            TriggerKind::Manual,
            vec!["m".into()],
        )
        .unwrap();
        let sidecar = toplevel
            .join("@snapshots")
            .join(format!("default-{}.meta.toml", info.id.as_str()));
        assert!(sidecar.exists());

        delete_snapshot(&config, &mock, &toplevel, &info).unwrap();
        assert!(
            !sidecar.exists(),
            "sidecar must be removed with the snapshot"
        );

        std::fs::remove_dir_all(&toplevel).ok();
    }

    // ----- resolve_live_parent -----
    //
    // Exercises the btrfs parent_uuid chain via the MockBackend, which
    // mirrors btrfs semantics: snapshot creation sets
    // `parent_uuid = source.uuid` on the child.

    /// Simulate a restore: rename the live subvol out of the way (so it
    /// is no longer the "live" one), then re-clone it from the given
    /// snapshot subvol at the original live path. After this call, the
    /// new live subvol has `parent_uuid == uuid_of(snap_path)`.
    fn simulate_restore(mock: &MockBackend, live_path: &Path, snap_path: &Path, delete_ts: &str) {
        let deleted = live_path.with_file_name(format!(
            "{}-DELETE-{}",
            live_path.file_name().unwrap().to_string_lossy(),
            delete_ts,
        ));
        mock.rename_subvolume(live_path, &deleted).unwrap();
        mock.create_writable_snapshot(snap_path, live_path).unwrap();
    }

    #[test]
    fn resolve_live_parent_pristine_returns_none() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        // No snapshots, live @ was created with no parent.
        assert!(resolve_live_parent(&config, &mock, toplevel).is_none());
    }

    #[test]
    fn resolve_live_parent_after_snapshot_returns_none() {
        // After a snapshot, the parent relationship is snap→live, not
        // live→snap. live.parent_uuid is still None.
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap();
        assert!(resolve_live_parent(&config, &mock, toplevel).is_none());
    }

    #[test]
    fn resolve_live_parent_after_restore_matches() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        let info = create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap();

        let snap_path = toplevel
            .join("@snapshots")
            .join(info.id.snapshot_name("@", "default"));
        simulate_restore(&mock, &toplevel.join("@"), &snap_path, "20260320-000000");

        let parent = resolve_live_parent(&config, &mock, toplevel).expect("should resolve");
        assert_eq!(parent.id, info.id);
        assert_eq!(parent.strain, "default");
    }

    #[test]
    fn resolve_live_parent_returns_none_when_parent_snapshot_deleted() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        let info = create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap();
        let snap_path = toplevel
            .join("@snapshots")
            .join(info.id.snapshot_name("@", "default"));
        simulate_restore(&mock, &toplevel.join("@"), &snap_path, "20260320-000000");

        // Parent snapshot is subsequently removed.
        delete_snapshot(&config, &mock, toplevel, &info).unwrap();

        assert!(resolve_live_parent(&config, &mock, toplevel).is_none());
    }

    #[test]
    fn resolve_live_parent_picks_correct_strain() {
        let config = config_two_strains(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Take one snapshot per strain, then restore from the periodic one.
        let default_snap = create_snapshot(
            &config,
            &mock,
            toplevel,
            "default",
            TriggerKind::Unknown,
            vec![],
        )
        .unwrap();
        // Different timestamp so the two snapshots never collide.
        let periodic_name = "@-periodic-20260316-150000";
        mock.seed_subvolume(toplevel.join("@snapshots").join(periodic_name));
        // Make its parent_uuid point at live @ so create_writable_snapshot
        // later produces a realistic parent relationship. The seed above
        // gave it a fresh uuid; we only need *its* uuid to match against
        // the restored live subvol.
        let periodic_path = toplevel.join("@snapshots").join(periodic_name);

        simulate_restore(
            &mock,
            &toplevel.join("@"),
            &periodic_path,
            "20260320-000000",
        );

        let parent = resolve_live_parent(&config, &mock, toplevel).expect("should resolve");
        assert_eq!(parent.strain, "periodic");
        assert_eq!(parent.id.as_str(), "20260316-150000");
        // Sanity: default snapshot exists but is not the live parent.
        assert_ne!(parent.id, default_snap.id);
    }

    #[test]
    fn snapshot_id_format() {
        let id = SnapshotId::now();
        assert_eq!(id.as_str().len(), ID_LEN_CURRENT);
        assert_eq!(id.as_str().as_bytes()[8], b'-');
        assert_eq!(id.as_str().as_bytes()[15], b'-');
        assert!(id.as_str()[16..].bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn snapshot_id_parse_current() {
        let id = SnapshotId::from_string("20260316-143022-456").unwrap();
        assert_eq!(id.as_str(), "20260316-143022-456");
    }

    #[test]
    fn snapshot_id_parse_legacy() {
        let id = SnapshotId::from_string("20260316-143022").unwrap();
        assert_eq!(id.as_str(), "20260316-143022");
    }

    #[test]
    fn snapshot_id_invalid() {
        assert!(SnapshotId::from_string("bad").is_err());
        assert!(SnapshotId::from_string("20261301-000000").is_err());
        // Non-digit ms suffix
        assert!(SnapshotId::from_string("20260316-143022-abc").is_err());
        // Wrong separator between seconds and ms
        assert!(SnapshotId::from_string("20260316-143022_456").is_err());
        // Too short / too long
        assert!(SnapshotId::from_string("20260316-14302").is_err());
        assert!(SnapshotId::from_string("20260316-143022-4567").is_err());
    }

    #[test]
    fn target_parses_strain_and_id() {
        let t: SnapshotTarget = "default@20260316-143022-456".parse().unwrap();
        let SnapshotTarget::Single { strain, id } = t else {
            panic!("expected Single");
        };
        assert_eq!(strain.as_deref(), Some("default"));
        assert_eq!(id.as_str(), "20260316-143022-456");
    }

    #[test]
    fn target_parses_bare_id_unscoped() {
        let t: SnapshotTarget = "20260316-143022-456".parse().unwrap();
        match t {
            SnapshotTarget::Single { strain: None, .. } => {}
            other => panic!("expected unscoped Single, got {other:?}"),
        }
    }

    #[test]
    fn target_parses_bulk_trailing_at() {
        let t: SnapshotTarget = "default@".parse().unwrap();
        assert_eq!(
            t,
            SnapshotTarget::AllInStrain {
                strain: "default".to_string(),
            }
        );
        assert!(t.is_bulk());
    }

    #[test]
    fn target_parses_bulk_all_alias() {
        // `strain@all` is accepted as a synonym for `strain@` so users
        // who dislike the empty-suffix form (or scripts where a missing
        // value would look like a bug) have a literal keyword.
        let t: SnapshotTarget = "default@all".parse().unwrap();
        assert_eq!(
            t,
            SnapshotTarget::AllInStrain {
                strain: "default".to_string(),
            }
        );
    }

    #[test]
    fn target_rejects_empty_strain() {
        let err = "@20260316-143022-456"
            .parse::<SnapshotTarget>()
            .unwrap_err();
        assert!(format!("{err}").contains("missing strain"));
    }

    #[test]
    fn target_rejects_invalid_strain_chars() {
        // `@` itself can't appear (split_once consumes the first one),
        // but other illegal chars in the strain slot must be caught.
        let err = "bad-name@20260316-143022-456"
            .parse::<SnapshotTarget>()
            .unwrap_err();
        assert!(format!("{err}").contains("invalid strain"), "got: {err}");
    }

    #[test]
    fn target_rejects_bare_non_id() {
        let err = "not-an-id".parse::<SnapshotTarget>().unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expected ID, strain@ID, or strain@"),
            "got: {msg}"
        );
    }

    #[test]
    fn target_rejects_invalid_id_in_qualified_form() {
        let err = "default@bogus".parse::<SnapshotTarget>().unwrap_err();
        assert!(format!("{err}").contains("invalid snapshot ID"));
    }

    #[test]
    fn target_display_round_trips() {
        let cases = [
            "default@20260316-143022-456",
            "20260316-143022-456",
            "default@",
        ];
        for s in cases {
            let t: SnapshotTarget = s.parse().unwrap();
            assert_eq!(format!("{t}"), s, "round-trip failed for {s:?}");
        }
        // `strain@all` Display normalises back to `strain@` — both
        // parse to the same variant, so the bulk form has one
        // canonical rendering.
        let t: SnapshotTarget = "default@all".parse().unwrap();
        assert_eq!(format!("{t}"), "default@");
    }

    #[test]
    fn qualified_helper_matches_target_display() {
        let id = SnapshotId::from_string("20260316-143022-456").unwrap();
        let s = qualified("default", &id);
        assert_eq!(s, "default@20260316-143022-456");
        let parsed: SnapshotTarget = s.parse().unwrap();
        assert!(matches!(
            parsed,
            SnapshotTarget::Single {
                strain: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn snapshot_id_orders_legacy_before_same_second_current() {
        // Legacy IDs lack the ms suffix and must sort before any
        // `<same second>-NNN` ID, so a strain that mixes formats is
        // still ordered chronologically within the same second.
        let legacy = SnapshotId::from_string("20260316-143022").unwrap();
        let current = SnapshotId::from_string("20260316-143022-000").unwrap();
        assert!(legacy < current);
    }

    #[test]
    fn snapshot_name_with_strain() {
        let id = SnapshotId::from_string("20260316-143022-456").unwrap();
        assert_eq!(
            id.snapshot_name("@", "default"),
            "@-default-20260316-143022-456"
        );
        assert_eq!(
            id.snapshot_name("@boot", "default"),
            "@boot-default-20260316-143022-456"
        );
    }

    #[test]
    fn snapshot_id_created_at_legacy() {
        let id = SnapshotId::from_string("20260316-143022").unwrap();
        let dt = id.created_at().unwrap();
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            "2026-03-16 14:30:22.000"
        );
    }

    #[test]
    fn snapshot_id_created_at_current() {
        let id = SnapshotId::from_string("20260316-143022-456").unwrap();
        let dt = id.created_at().unwrap();
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            "2026-03-16 14:30:22.456"
        );
    }

    #[test]
    fn extract_trailing_prefers_current_over_legacy() {
        // A subvolume name carrying a current-form ID must parse as
        // current, not as legacy with a 4-digit "ms" component bleeding
        // into the strain.
        let (id, start) = SnapshotId::extract_trailing("@-default-20260316-143022-456").unwrap();
        assert_eq!(id.as_str(), "20260316-143022-456");
        assert_eq!(start, "@-default-".len());
    }

    #[test]
    fn extract_trailing_falls_back_to_legacy() {
        let (id, start) = SnapshotId::extract_trailing("@-default-20260316-143022").unwrap();
        assert_eq!(id.as_str(), "20260316-143022");
        assert_eq!(start, "@-default-".len());
    }

    // ----- parse_snapshot_subvol_name -----

    #[test]
    fn parse_subvol_name_simple() {
        let (subvol, strain, id) =
            parse_snapshot_subvol_name("@-default-20260316-143022-456").unwrap();
        assert_eq!(subvol, "@");
        assert_eq!(strain, "default");
        assert_eq!(id.as_str(), "20260316-143022-456");
    }

    #[test]
    fn parse_subvol_name_legacy_id() {
        let (subvol, strain, id) =
            parse_snapshot_subvol_name("@home-default-20260316-143022").unwrap();
        assert_eq!(subvol, "@home");
        assert_eq!(strain, "default");
        assert_eq!(id.as_str(), "20260316-143022");
    }

    #[test]
    fn parse_subvol_name_subvol_with_hyphen() {
        // Subvol names may contain hyphens; strain names may not, so the
        // last `-` before the id separator is unambiguously the boundary.
        let (subvol, strain, _) =
            parse_snapshot_subvol_name("@my-thing-default-20260316-143022-456").unwrap();
        assert_eq!(subvol, "@my-thing");
        assert_eq!(strain, "default");
    }

    #[test]
    fn parse_subvol_name_rejects_garbage() {
        assert!(parse_snapshot_subvol_name("random-thing").is_none());
        assert!(parse_snapshot_subvol_name("20260316-143022").is_none());
        assert!(parse_snapshot_subvol_name("@-default-20260316-bogus").is_none());
        // Empty subvol or strain
        assert!(parse_snapshot_subvol_name("-default-20260316-143022").is_none());
        assert!(parse_snapshot_subvol_name("@--20260316-143022").is_none());
    }

    #[test]
    fn parse_subvol_name_round_trips_snapshot_name() {
        let id = SnapshotId::from_string("20260316-143022-456").unwrap();
        let name = id.snapshot_name("@home", "periodic");
        let (subvol, strain, parsed_id) = parse_snapshot_subvol_name(&name).unwrap();
        assert_eq!(subvol, "@home");
        assert_eq!(strain, "periodic");
        assert_eq!(parsed_id, id);
    }
}
