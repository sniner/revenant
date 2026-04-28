use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, NaiveDateTime, Timelike, Utc};
use serde::Serialize;

use crate::backend::{FileSystemBackend, subvol_exists};
use crate::config::Config;
use crate::error::{Result, RevenantError};
use crate::metadata::{self, SnapshotMetadata, TriggerKind};

/// Snapshot identifier based on UTC timestamp: `YYYYMMDD-HHMMSS-NNN`,
/// where the trailing three digits are milliseconds. Legacy IDs without
/// the millisecond suffix (`YYYYMMDD-HHMMSS`) are still accepted on read,
/// so existing snapshots remain valid; new IDs are always emitted in the
/// extended form.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SnapshotId(String);

/// Length of a legacy snapshot ID, `YYYYMMDD-HHMMSS`.
const ID_LEN_LEGACY: usize = 15;

/// Length of the current snapshot ID, `YYYYMMDD-HHMMSS-NNN`.
const ID_LEN_CURRENT: usize = 19;

impl SnapshotId {
    /// Generate a new snapshot ID from the current UTC time, including
    /// millisecond precision so that snapshots created back-to-back in
    /// the same strain do not collide.
    #[must_use]
    pub fn now() -> Self {
        let ts = Utc::now().format("%Y%m%d-%H%M%S-%3f").to_string();
        Self(ts)
    }

    /// Create a snapshot ID from a known string (e.g. parsed from subvolume name).
    /// Accepts both the legacy 15-char form and the current 19-char form.
    pub fn from_string(s: &str) -> std::result::Result<Self, RevenantError> {
        match s.len() {
            ID_LEN_CURRENT => {
                if s.as_bytes()[8] != b'-' || s.as_bytes()[15] != b'-' {
                    return Err(RevenantError::Other(format!(
                        "invalid snapshot ID format: {s}"
                    )));
                }
                let ms = &s[16..];
                if !ms.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(RevenantError::Other(format!(
                        "invalid snapshot ID milliseconds: {s}"
                    )));
                }
                NaiveDateTime::parse_from_str(&s[..ID_LEN_LEGACY], "%Y%m%d-%H%M%S").map_err(
                    |_| RevenantError::Other(format!("invalid snapshot ID timestamp: {s}")),
                )?;
            }
            ID_LEN_LEGACY => {
                if s.as_bytes()[8] != b'-' {
                    return Err(RevenantError::Other(format!(
                        "invalid snapshot ID format: {s}"
                    )));
                }
                NaiveDateTime::parse_from_str(s, "%Y%m%d-%H%M%S").map_err(|_| {
                    RevenantError::Other(format!("invalid snapshot ID timestamp: {s}"))
                })?;
            }
            _ => {
                return Err(RevenantError::Other(format!(
                    "invalid snapshot ID format: {s}"
                )));
            }
        }
        Ok(Self(s.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derive a UTC `DateTime` from the embedded timestamp. The
    /// millisecond suffix, if any, is preserved in the returned value.
    #[must_use]
    pub fn created_at(&self) -> Option<DateTime<Utc>> {
        let (secs, ms) = if self.0.len() == ID_LEN_CURRENT {
            (
                &self.0[..ID_LEN_LEGACY],
                self.0[ID_LEN_LEGACY + 1..].parse::<u32>().ok()?,
            )
        } else {
            (self.0.as_str(), 0)
        };
        let dt = NaiveDateTime::parse_from_str(secs, "%Y%m%d-%H%M%S")
            .ok()?
            .and_utc();
        dt.with_nanosecond(ms.checked_mul(1_000_000)?)
    }

    /// Build the snapshot subvolume name for a given source subvolume and strain.
    /// E.g. source "@", strain "default", id "20260316-143022-456" →
    /// "@-default-20260316-143022-456".
    #[must_use]
    pub fn snapshot_name(&self, subvol: &str, strain: &str) -> String {
        format!("{subvol}-{strain}-{}", self.0)
    }

    /// Extract a trailing snapshot ID from a string like
    /// `"...-<strain>-<id>"`. Tries the current 19-char form first, then
    /// the legacy 15-char form. Returns the parsed ID and the byte index
    /// in `s` at which the ID begins (the byte at `start - 1` is the
    /// `'-'` separator).
    #[must_use]
    pub fn extract_trailing(s: &str) -> Option<(Self, usize)> {
        for &width in &[ID_LEN_CURRENT, ID_LEN_LEGACY] {
            if s.len() < width + 2 {
                continue;
            }
            let start = s.len() - width;
            if s.as_bytes()[start - 1] != b'-' {
                continue;
            }
            if let Ok(id) = Self::from_string(&s[start..]) {
                return Some((id, start));
            }
        }
        None
    }
}

/// Parse a snapshot subvolume name back into its `(subvol, strain, id)`
/// components — the inverse of [`SnapshotId::snapshot_name`].
///
/// Strain names are constrained to `[a-zA-Z0-9_]` (no hyphens), so the
/// last `-` before the trailing id separator is the subvol/strain
/// boundary. Returns `None` for anything that isn't shaped like a
/// snapshot subvolume name.
#[must_use]
pub fn parse_snapshot_subvol_name(name: &str) -> Option<(String, String, SnapshotId)> {
    let (id, id_start) = SnapshotId::extract_trailing(name)?;
    // `prefix` is "<subvol>-<strain>"; id_start points at the byte after
    // the trailing '-' separator.
    let prefix = &name[..id_start - 1];
    let dash = prefix.rfind('-')?;
    let subvol = &prefix[..dash];
    let strain = &prefix[dash + 1..];
    if subvol.is_empty() || strain.is_empty() {
        return None;
    }
    if !strain
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    Some((subvol.to_string(), strain.to_string(), id))
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0)
    }
}

impl FromStr for SnapshotId {
    type Err = RevenantError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::from_string(s)
    }
}

/// A user-supplied snapshot reference for `restore`/`delete`/`list`.
///
/// The textual form is the canonical addressing notation across the CLI:
///
/// * `strain@ID` — fully qualified single snapshot
/// * `ID` — single snapshot, strain auto-resolved by lookup
/// * `strain@` — every snapshot in a strain (bulk)
/// * `strain@all` — alias for `strain@`, kept because an empty ID slot
///   looks like a missing argument in scripts
///
/// Strain names are restricted to `[a-zA-Z0-9_]` (see
/// [`crate::config`]'s validation), so `@` is unambiguous as the
/// separator and we split on the *first* one we see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotTarget {
    /// One specific snapshot. `strain` is `Some` when the user spelled
    /// it out, `None` for a bare ID — the resolver looks it up across
    /// strains and errors if it is ambiguous.
    Single {
        strain: Option<String>,
        id: SnapshotId,
    },
    /// Every snapshot of a given strain.
    AllInStrain { strain: String },
}

impl SnapshotTarget {
    /// `true` for the bulk variant (`strain@` / `strain@all`).
    #[must_use]
    pub fn is_bulk(&self) -> bool {
        matches!(self, Self::AllInStrain { .. })
    }
}

impl fmt::Display for SnapshotTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Single {
                strain: Some(s),
                id,
            } => write!(f, "{s}@{id}"),
            Self::Single { strain: None, id } => write!(f, "{id}"),
            Self::AllInStrain { strain } => write!(f, "{strain}@"),
        }
    }
}

impl FromStr for SnapshotTarget {
    type Err = RevenantError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(RevenantError::Other(
                "empty snapshot target — expected ID, strain@ID, or strain@".to_string(),
            ));
        }
        match s.split_once('@') {
            Some((strain, rest)) => {
                if strain.is_empty() {
                    return Err(RevenantError::Other(format!(
                        "missing strain before '@' in {s:?}"
                    )));
                }
                if !is_valid_strain_token(strain) {
                    return Err(RevenantError::Other(format!(
                        "invalid strain name {strain:?} in target — only [a-zA-Z0-9_] allowed"
                    )));
                }
                if rest.is_empty() || rest == "all" {
                    Ok(Self::AllInStrain {
                        strain: strain.to_string(),
                    })
                } else {
                    let id = SnapshotId::from_string(rest).map_err(|e| {
                        RevenantError::Other(format!("invalid snapshot ID in {s:?}: {e}"))
                    })?;
                    Ok(Self::Single {
                        strain: Some(strain.to_string()),
                        id,
                    })
                }
            }
            None => {
                let id = SnapshotId::from_string(s).map_err(|_| {
                    RevenantError::Other(format!("expected ID, strain@ID, or strain@ — got {s:?}"))
                })?;
                Ok(Self::Single { strain: None, id })
            }
        }
    }
}

/// Local mirror of the strain-name predicate used by `Config::validate`.
/// Duplicated here to avoid a `config` ↔ `snapshot` import dependency for
/// what is a one-line predicate; the canonical rule lives in `config.rs`.
fn is_valid_strain_token(s: &str) -> bool {
    !s.is_empty()
        && s != crate::config::DELETE_STRAIN
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

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
fn snapshot_dir(config: &Config, toplevel: &Path) -> PathBuf {
    toplevel.join(&config.sys.snapshot_subvol)
}

/// Compute the sidecar metadata path for a snapshot. The sidecar is
/// keyed on `(strain, id)` only, so reordering the strain's
/// `subvolumes = [...]` list does not orphan existing metadata.
fn sidecar_path_for_snapshot(snap_dir: &Path, strain: &str, id: &SnapshotId) -> PathBuf {
    metadata::sidecar_path(snap_dir, strain, id.as_str())
}

/// Ensure the snapshot subvolume exists, creating it if necessary.
fn ensure_snapshot_dir(
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

    let sidecar = sidecar_path_for_snapshot(&snap_dir, &snapshot.strain, &snapshot.id);
    if let Err(e) = metadata::remove(&sidecar) {
        tracing::warn!("failed to remove metadata {}: {e}", sidecar.display());
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
