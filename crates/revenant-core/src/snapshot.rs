use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;

use crate::backend::{FileSystemBackend, subvol_exists};
use crate::config::Config;
use crate::error::{Result, RevenantError};

/// Snapshot identifier based on UTC timestamp: YYYYMMDD-HHMMSS.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// Generate a new snapshot ID from the current UTC time.
    #[must_use]
    pub fn now() -> Self {
        let ts = Utc::now().format("%Y%m%d-%H%M%S").to_string();
        Self(ts)
    }

    /// Create a snapshot ID from a known string (e.g. parsed from subvolume name).
    pub fn from_string(s: &str) -> std::result::Result<Self, RevenantError> {
        // Validate format: YYYYMMDD-HHMMSS
        if s.len() != 15 || s.as_bytes()[8] != b'-' {
            return Err(RevenantError::Other(format!(
                "invalid snapshot ID format: {s}"
            )));
        }
        // Verify parseable as a timestamp
        NaiveDateTime::parse_from_str(s, "%Y%m%d-%H%M%S")
            .map_err(|_| RevenantError::Other(format!("invalid snapshot ID timestamp: {s}")))?;
        Ok(Self(s.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derive a UTC `DateTime` from the embedded timestamp.
    #[must_use]
    pub fn created_at(&self) -> Option<DateTime<Utc>> {
        NaiveDateTime::parse_from_str(&self.0, "%Y%m%d-%H%M%S")
            .ok()
            .map(|dt| dt.and_utc())
    }

    /// Build the snapshot subvolume name for a given source subvolume and strain.
    /// E.g. source "@", strain "default", id "20260316-143022" → "@-default-20260316-143022"
    #[must_use]
    pub fn snapshot_name(&self, subvol: &str, strain: &str) -> String {
        format!("{subvol}-{strain}-{}", self.0)
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SnapshotId {
    type Err = RevenantError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::from_string(s)
    }
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
}

/// Return the path to the snapshot subvolume within the toplevel.
fn snapshot_dir(config: &Config, toplevel: &Path) -> std::path::PathBuf {
    toplevel.join(&config.sys.snapshot_subvol)
}

/// Ensure the snapshot subvolume exists, creating it if necessary.
fn ensure_snapshot_dir(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<std::path::PathBuf> {
    let dir = snapshot_dir(config, toplevel);
    if !subvol_exists(backend, &dir) {
        tracing::info!("creating snapshot subvolume {}", config.sys.snapshot_subvol);
        backend.create_subvolume(&dir)?;
    }
    Ok(dir)
}

/// Discover all snapshots by scanning subvolumes on disk and matching against config.
///
/// For each configured strain and its subvolumes, looks for subvolume names matching
/// `{subvol}-{strain}-{YYYYMMDD-HHMMSS}`, groups by (strain, timestamp), and returns
/// the list sorted chronologically.
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
            Some(SnapshotInfo {
                id,
                strain,
                subvolumes: subvols,
                efi_synced,
            })
        })
        .collect();

    snapshots.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.strain.cmp(&b.strain)));
    Ok(snapshots)
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
            let strains: Vec<_> = matches.iter().map(|s| s.strain.as_str()).collect();
            Err(RevenantError::Other(format!(
                "snapshot {id} exists in multiple strains: {}. Use --strain to disambiguate.",
                strains.join(", ")
            )))
        }
    }
}

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

    let info = SnapshotInfo {
        id,
        strain: strain_name.to_string(),
        subvolumes: snapshotted_subvols,
        efi_synced,
    };

    tracing::info!("snapshot {} created successfully", info.id);
    Ok(info)
}

/// Delete a snapshot and all its associated subvolumes.
pub fn delete_snapshot(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    snapshot: &SnapshotInfo,
) -> Result<()> {
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

    tracing::info!("snapshot {} deleted", snapshot.id);
    Ok(())
}

/// Delete all snapshots belonging to a given strain.
pub fn delete_all_strain(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    strain_name: &str,
) -> Result<Vec<String>> {
    let all = discover_snapshots(config, backend, toplevel)?;
    let strain_snapshots: Vec<_> = all
        .into_iter()
        .filter(|s| s.strain == strain_name)
        .collect();

    let mut removed = Vec::new();
    for snap in &strain_snapshots {
        tracing::info!("deleting snapshot {} (strain: {strain_name})", snap.id);
        delete_snapshot(config, backend, toplevel, snap)?;
        removed.push(snap.id.to_string());
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

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

        let info = create_snapshot(&config, &mock, toplevel, "default").unwrap();
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

        let _ = create_snapshot(&config, &mock, toplevel, "default").unwrap();
        assert!(mock.contains(toplevel.join("@snapshots")));
    }

    #[test]
    fn create_snapshot_multi_subvol() {
        let config = config_no_efi(&["@", "@home"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));
        mock.seed_subvolume(toplevel.join("@home"));

        let info = create_snapshot(&config, &mock, toplevel, "default").unwrap();
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

        let err = create_snapshot(&config, &mock, toplevel, "nonexistent").unwrap_err();
        assert!(format!("{err}").contains("nonexistent"));
    }

    #[test]
    fn create_snapshot_then_discover_round_trip() {
        let config = config_no_efi(&["@"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        mock.seed_subvolume(toplevel.join("@"));

        let info = create_snapshot(&config, &mock, toplevel, "default").unwrap();
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

        let info = create_snapshot(&config, &mock, toplevel, "default").unwrap();
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

        let info = create_snapshot(&config, &mock, toplevel, "default").unwrap();
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
        assert_eq!(removed.len(), 2);

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
        assert!(removed.is_empty());
    }

    #[test]
    fn snapshot_id_format() {
        let id = SnapshotId::now();
        assert_eq!(id.as_str().len(), 15);
        assert_eq!(id.as_str().as_bytes()[8], b'-');
    }

    #[test]
    fn snapshot_id_parse() {
        let id = SnapshotId::from_string("20260316-143022").unwrap();
        assert_eq!(id.as_str(), "20260316-143022");
    }

    #[test]
    fn snapshot_id_invalid() {
        assert!(SnapshotId::from_string("bad").is_err());
        assert!(SnapshotId::from_string("20261301-000000").is_err());
    }

    #[test]
    fn snapshot_name_with_strain() {
        let id = SnapshotId::from_string("20260316-143022").unwrap();
        assert_eq!(
            id.snapshot_name("@", "default"),
            "@-default-20260316-143022"
        );
        assert_eq!(
            id.snapshot_name("@boot", "default"),
            "@boot-default-20260316-143022"
        );
    }

    #[test]
    fn snapshot_id_created_at() {
        let id = SnapshotId::from_string("20260316-143022").unwrap();
        let dt = id.created_at().unwrap();
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-03-16 14:30:22"
        );
    }
}
