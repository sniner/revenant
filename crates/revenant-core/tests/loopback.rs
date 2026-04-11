//! Integration tests against a real loopback btrfs filesystem.
//!
//! These tests require root and the standard btrfs userspace tools, and
//! must be run via `tools/run-loopback-tests.sh` which sets up a private
//! mount namespace. They are gated behind the `loopback-tests` Cargo
//! feature so plain `cargo test` skips them entirely.
//!
//! What's covered here that the mock tests can't catch:
//! - Real ioctl behaviour, including the readonly-flag dance in
//!   `delete_subvolume` and the BTRFS_IOC_SUBVOL_CREATE/SNAP_CREATE_V2 path
//! - `find_nested_subvolumes` walking a real directory tree
//! - End-to-end orchestration (`create_snapshot` → `restore_snapshot`)
//!   against a real backend, verifying that the live data actually changes

#![cfg(feature = "loopback-tests")]

mod common;

use common::TestFs;
use revenant_core::backend::FileSystemBackend;
use revenant_core::backend::btrfs::BtrfsBackend;
use revenant_core::config::Config;
use revenant_core::restore::restore_snapshot;
use revenant_core::snapshot::{create_snapshot, find_snapshot};

// ----- low-level ioctl wrappers -----

#[test]
fn probe_recognises_btrfs() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    assert!(
        backend.probe(&fs.mount).unwrap(),
        "loopback fs should probe as btrfs"
    );
}

#[test]
fn create_subvolume_then_inspect() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let p = fs.mount.join("@root");

    backend.create_subvolume(&p).unwrap();

    let info = backend.subvolume_info(&p).unwrap();
    assert!(!info.readonly, "fresh subvolume should be writable");
    assert_eq!(info.path, p);
    assert_ne!(info.id, 0);
}

#[test]
fn list_subvolumes_returns_direct_children() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    backend.create_subvolume(&fs.mount.join("@a")).unwrap();
    backend.create_subvolume(&fs.mount.join("@b")).unwrap();
    backend.create_subvolume(&fs.mount.join("@c")).unwrap();

    let listed = backend.list_subvolumes(&fs.mount).unwrap();
    let mut names: Vec<String> = listed
        .iter()
        .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["@a", "@b", "@c"]);
}

#[test]
fn readonly_snapshot_is_readonly() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let src = fs.mount.join("@");
    let snap = fs.mount.join("@snap");

    backend.create_subvolume(&src).unwrap();
    backend.create_readonly_snapshot(&src, &snap).unwrap();

    let info = backend.subvolume_info(&snap).unwrap();
    assert!(info.readonly, "ro snapshot must report readonly=true");
}

#[test]
fn writable_snapshot_is_writable() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let src = fs.mount.join("@");
    let snap = fs.mount.join("@snap-rw");

    backend.create_subvolume(&src).unwrap();
    backend.create_writable_snapshot(&src, &snap).unwrap();

    let info = backend.subvolume_info(&snap).unwrap();
    assert!(!info.readonly, "rw snapshot must report readonly=false");
    // And we should actually be able to write to it.
    std::fs::write(snap.join("touch.txt"), "ok").unwrap();
}

#[test]
fn snapshot_captures_content_at_creation_time() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let src = fs.mount.join("@");
    let snap = fs.mount.join("@snap");

    backend.create_subvolume(&src).unwrap();
    std::fs::write(src.join("file.txt"), "version 1").unwrap();

    backend.create_readonly_snapshot(&src, &snap).unwrap();

    // Mutate the live subvolume *after* the snapshot.
    std::fs::write(src.join("file.txt"), "version 2").unwrap();

    // Snapshot still has the original content.
    let snapped = std::fs::read_to_string(snap.join("file.txt")).unwrap();
    assert_eq!(snapped, "version 1");
}

#[test]
fn delete_readonly_subvolume_clears_flag_first() {
    // Regression-critical: btrfs refuses to delete a readonly subvolume,
    // so delete_subvolume must clear the readonly flag before destroying.
    // The mock cannot exercise this; only real ioctls can.
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let src = fs.mount.join("@");
    let snap = fs.mount.join("@ro-snap");

    backend.create_subvolume(&src).unwrap();
    backend.create_readonly_snapshot(&src, &snap).unwrap();

    backend
        .delete_subvolume(&snap)
        .expect("delete must clear readonly flag and succeed");
    assert!(
        backend.subvolume_info(&snap).is_err(),
        "subvolume should be gone after delete"
    );
}

#[test]
fn delete_writable_subvolume() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let p = fs.mount.join("@");
    backend.create_subvolume(&p).unwrap();

    backend.delete_subvolume(&p).unwrap();
    assert!(backend.subvolume_info(&p).is_err());
}

#[test]
fn rename_subvolume_moves_directory_entry() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let a = fs.mount.join("@a");
    let b = fs.mount.join("@b");

    backend.create_subvolume(&a).unwrap();
    std::fs::write(a.join("marker"), "hi").unwrap();

    backend.rename_subvolume(&a, &b).unwrap();

    assert!(backend.subvolume_info(&a).is_err(), "old name must be gone");
    let info = backend.subvolume_info(&b).unwrap();
    assert_eq!(info.path, b);
    assert!(!info.readonly);
    // Content carried across the rename.
    assert_eq!(std::fs::read_to_string(b.join("marker")).unwrap(), "hi");
}

#[test]
fn set_default_subvolume_does_not_error() {
    // Verifying the default subvolume took effect would require remounting
    // the filesystem. We at least confirm the ioctl call returns success.
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let p = fs.mount.join("@");
    backend.create_subvolume(&p).unwrap();
    backend.set_default_subvolume(&p).unwrap();
}

#[test]
fn find_nested_subvolumes_walks_real_tree() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let root = fs.mount.join("@");
    backend.create_subvolume(&root).unwrap();

    // Plain dirs interleaved with nested subvolumes.
    std::fs::create_dir_all(root.join("var/lib")).unwrap();
    backend
        .create_subvolume(&root.join("var/lib/portables"))
        .unwrap();
    backend
        .create_subvolume(&root.join("var/lib/machines"))
        .unwrap();
    std::fs::create_dir_all(root.join("home/user")).unwrap();

    let nested = backend.find_nested_subvolumes(&root).unwrap();
    assert_eq!(
        nested.len(),
        2,
        "expected exactly two nested subvolumes, got: {nested:?}"
    );
    assert!(nested.iter().any(|p| p.ends_with("portables")));
    assert!(nested.iter().any(|p| p.ends_with("machines")));
}

// ----- end-to-end orchestration -----

/// Build a no-EFI single-strain config that snapshots `@` into `@snapshots`.
fn e2e_config() -> Config {
    let toml = r#"
[sys]
rootfs_subvol = "@"
snapshot_subvol = "@snapshots"

[sys.rootfs]
backend = "btrfs"
device_uuid = "00000000-0000-0000-0000-000000000000"

[sys.efi]
enabled = false

[sys.bootloader]
backend = "systemd-boot"

[strain.default]
subvolumes = ["@"]

[strain.default.retain]
last = 5
"#;
    toml.parse().unwrap()
}

/// Sleep at least one full second so two consecutive `SnapshotId::now()`
/// calls are guaranteed distinct.
fn distinct_second() {
    std::thread::sleep(std::time::Duration::from_millis(1100));
}

#[test]
fn e2e_create_snapshot_against_real_backend() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let config = e2e_config();

    backend.create_subvolume(&fs.mount.join("@")).unwrap();
    std::fs::write(fs.mount.join("@/hello.txt"), "world").unwrap();

    let info = create_snapshot(&config, &backend, &fs.mount, "default").unwrap();
    assert_eq!(info.strain, "default");
    assert_eq!(info.subvolumes, vec!["@".to_string()]);

    // The snapshot subvolume should now exist with our content frozen in.
    let snap_path = fs
        .mount
        .join("@snapshots")
        .join(info.id.snapshot_name("@", "default"));
    let snap_info = backend.subvolume_info(&snap_path).unwrap();
    assert!(snap_info.readonly);
    assert_eq!(
        std::fs::read_to_string(snap_path.join("hello.txt")).unwrap(),
        "world"
    );
}

#[test]
fn e2e_restore_rolls_back_live_state() {
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let config = e2e_config();

    let root = fs.mount.join("@");
    backend.create_subvolume(&root).unwrap();
    std::fs::write(root.join("state.txt"), "before").unwrap();

    // Capture the "before" state.
    let snap = create_snapshot(&config, &backend, &fs.mount, "default").unwrap();
    let snap_id = snap.id.clone();

    distinct_second();

    // Modify the live subvolume.
    std::fs::write(root.join("state.txt"), "after").unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("state.txt")).unwrap(),
        "after"
    );

    // Look up the snapshot freshly via the backend (mirrors the CLI flow)
    // and restore it.
    let info = find_snapshot(&config, &backend, &fs.mount, &snap_id, Some("default")).unwrap();
    restore_snapshot(&config, &backend, &fs.mount, &info).unwrap();

    // After restore, @ has been replaced — the live state must read "before".
    assert_eq!(
        std::fs::read_to_string(root.join("state.txt")).unwrap(),
        "before",
        "live subvolume should reflect restored snapshot content"
    );

    // The DELETE marker for the previous live state should be present in
    // the toplevel.
    let listed = backend.list_subvolumes(&fs.mount).unwrap();
    let has_marker = listed.iter().any(|s| {
        s.path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("@-DELETE-"))
    });
    assert!(has_marker, "expected @-DELETE-* marker after restore");
}

#[test]
fn e2e_restore_preserves_nested_subvol_data() {
    // Headline use case for the nested-subvol re-attach refactor: a
    // stock Arch system has nested subvolumes inside @
    // (var/lib/portables, var/lib/machines) holding runtime state
    // that should NOT get rolled back when @ is restored.  The
    // nested subvol must survive the restore at its *current* state,
    // not at the snapshot's state.
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let config = e2e_config();

    let root = fs.mount.join("@");
    backend.create_subvolume(&root).unwrap();
    std::fs::write(root.join("state.txt"), "before").unwrap();

    // Nested subvol with some data inside @.
    std::fs::create_dir_all(root.join("var/lib")).unwrap();
    let nested = root.join("var/lib/portables");
    backend.create_subvolume(&nested).unwrap();
    std::fs::write(nested.join("data.txt"), "v1").unwrap();

    // Snapshot @.  btrfs does not recurse into nested subvols, so
    // the nested portables data is NOT captured here.
    let snap = create_snapshot(&config, &backend, &fs.mount, "default").unwrap();
    let snap_id = snap.id.clone();

    distinct_second();

    // Mutate both @ and the nested subvol after the snapshot.
    std::fs::write(root.join("state.txt"), "after").unwrap();
    std::fs::write(nested.join("data.txt"), "v2").unwrap();

    // Restore @ from the captured snapshot.
    let info = find_snapshot(&config, &backend, &fs.mount, &snap_id, Some("default")).unwrap();
    restore_snapshot(&config, &backend, &fs.mount, &info).unwrap();

    // @'s top-level state must be rolled back …
    assert_eq!(
        std::fs::read_to_string(root.join("state.txt")).unwrap(),
        "before",
        "live @ should reflect restored snapshot"
    );

    // … but the nested subvolume must still exist with the LATEST
    // (post-snapshot) data.  This is the whole point of the
    // refactor: nested-subvol state is treated as runtime data, not
    // versioned content.
    let nested_after = root.join("var/lib/portables");
    let nested_info = backend
        .subvolume_info(&nested_after)
        .expect("nested subvol must still exist after restore");
    assert!(!nested_info.readonly);
    assert_eq!(
        std::fs::read_to_string(nested_after.join("data.txt")).unwrap(),
        "v2",
        "nested data must be preserved at its current state, not rolled back"
    );
}

#[test]
fn e2e_restore_creates_parent_path_for_nested_added_after_snapshot() {
    // Cross-restore-boundary case: roll back to a snapshot taken
    // BEFORE the nested subvolume's parent directories ever existed.
    // restore_snapshot has to materialise the missing parent path on
    // the fly so the nested subvol has somewhere to land — otherwise
    // the data would be stranded in the DELETE marker, and a
    // subsequent roll-forward would never see it again.
    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let config = e2e_config();

    let root = fs.mount.join("@");
    backend.create_subvolume(&root).unwrap();
    std::fs::write(root.join("state.txt"), "before").unwrap();

    // Snapshot now — note: no var/ tree at all yet.
    let snap = create_snapshot(&config, &backend, &fs.mount, "default").unwrap();
    let snap_id = snap.id.clone();

    distinct_second();

    // After the snapshot: install a nested subvol whose parent path
    // doesn't exist in the snapshot.
    std::fs::create_dir_all(root.join("var/lib")).unwrap();
    let nested = root.join("var/lib/portables");
    backend.create_subvolume(&nested).unwrap();
    std::fs::write(nested.join("data.txt"), "later").unwrap();
    std::fs::write(root.join("state.txt"), "after").unwrap();

    // Restore the snapshot — rollback that pre-dates the nested
    // subvol's parent path.
    let info = find_snapshot(&config, &backend, &fs.mount, &snap_id, Some("default")).unwrap();
    restore_snapshot(&config, &backend, &fs.mount, &info).unwrap();

    // @'s top-level state rolled back …
    assert_eq!(
        std::fs::read_to_string(root.join("state.txt")).unwrap(),
        "before"
    );

    // … and the nested subvolume that DIDN'T exist in the snapshot
    // is nevertheless preserved at its current state in the new @.
    // The parent path was materialised on the fly by the backend's
    // create_dir_all.
    let nested_after = root.join("var/lib/portables");
    backend
        .subvolume_info(&nested_after)
        .expect("nested subvol must survive rollback across the install boundary");
    assert_eq!(
        std::fs::read_to_string(nested_after.join("data.txt")).unwrap(),
        "later"
    );
}

#[test]
fn e2e_recovery_hook_rescues_orphaned_nested_from_delete_marker() {
    // Simulate a previous restore that crashed between the rename of
    // @ and the re-attach loop: the nested subvolume is stranded
    // inside an @-DELETE-{ts} marker.  The recovery hook
    // (`recover_orphaned_nested_subvols`) that runs at the start of
    // every write command must pick it up and move it back into the
    // freshly recreated @.
    use revenant_core::cleanup::recover_orphaned_nested_subvols;

    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let config = e2e_config();

    // Live @ with a nested subvol that holds important data.
    let root = fs.mount.join("@");
    backend.create_subvolume(&root).unwrap();
    std::fs::create_dir_all(root.join("var/lib")).unwrap();
    let nested = root.join("var/lib/portables");
    backend.create_subvolume(&nested).unwrap();
    std::fs::write(nested.join("payload"), "important").unwrap();

    // Simulate the interrupted restore: rename @ to a DELETE marker
    // (the nested subvol rides along automatically because its
    // directory entry lives in @'s tree) and create a fresh empty @
    // in its place.  This is exactly the on-disk state restore_snapshot
    // would leave behind if it crashed right between the rename and
    // the re-attach loop.
    let marker = fs.mount.join("@-DELETE-20260101-120000");
    backend.rename_subvolume(&root, &marker).unwrap();
    backend.create_subvolume(&root).unwrap();

    // Pre-conditions: nested data lives in the marker, not in @.
    backend
        .subvolume_info(&marker.join("var/lib/portables"))
        .expect("nested should be inside marker before recovery");
    assert!(
        backend
            .subvolume_info(&root.join("var/lib/portables"))
            .is_err()
    );

    // Run recovery.
    let recovered = recover_orphaned_nested_subvols(&config, &backend, &fs.mount).unwrap();
    assert_eq!(recovered, 1, "expected exactly one nested subvol rescued");

    // The nested subvol moved out of the marker and into @.
    backend
        .subvolume_info(&root.join("var/lib/portables"))
        .expect("nested must be inside @ after recovery");
    assert!(
        backend
            .subvolume_info(&marker.join("var/lib/portables"))
            .is_err(),
        "nested must no longer be inside the marker after recovery"
    );

    // Data preserved across the move.
    assert_eq!(
        std::fs::read_to_string(root.join("var/lib/portables/payload")).unwrap(),
        "important"
    );

    // Marker still exists (purge handles its removal in the next
    // cleanup pass once it's empty of nested subvolumes).
    backend
        .subvolume_info(&marker)
        .expect("marker stays in place — purge cleans it up next");
}

#[test]
fn e2e_restore_rejects_incomplete_snapshot() {
    use revenant_core::error::RevenantError;
    use revenant_core::snapshot::{SnapshotId, SnapshotInfo};

    let fs = TestFs::new();
    let backend = BtrfsBackend::new();
    let config = e2e_config();

    backend.create_subvolume(&fs.mount.join("@")).unwrap();
    backend
        .create_subvolume(&fs.mount.join("@snapshots"))
        .unwrap();

    // Build a SnapshotInfo for an ID that doesn't exist on disk at all.
    let bogus = SnapshotInfo {
        id: SnapshotId::from_string("20990101-000000").unwrap(),
        strain: "default".to_string(),
        subvolumes: vec!["@".to_string()],
        efi_synced: false,
    };

    let err = restore_snapshot(&config, &backend, &fs.mount, &bogus).unwrap_err();
    assert!(
        matches!(err, RevenantError::IncompleteSnapshot { .. }),
        "expected IncompleteSnapshot, got {err:?}"
    );

    // And the live @ should be untouched.
    assert!(backend.subvolume_info(&fs.mount.join("@")).is_ok());
}
