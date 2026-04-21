//! Per-snapshot metadata sidecar files.
//!
//! A snapshot is identified by (strain, id); the id is a UTC timestamp
//! encoded in the subvolume name. That is enough to list snapshots, but not
//! enough to tell a user *why* a snapshot exists or what triggered it. This
//! module stores optional metadata in a small TOML file inside the snapshot
//! directory, named `<strain>-<id>.meta.toml`.
//!
//! The sidecar is keyed on (strain, id) rather than on any particular
//! snapshot subvolume name, so reordering `subvolumes = [...]` in the
//! configuration does not orphan existing metadata.
//!
//! The sidecar is optional on read: a missing file simply means "no
//! metadata" and the snapshot is still listed. Writers must not fail the
//! snapshot creation when the sidecar cannot be written — metadata loss is
//! always preferable to a stranded half-created snapshot.

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, Local, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::backend::{FileSystemBackend, subvol_exists};
use crate::error::{Result, RevenantError};

pub const SCHEMA_VERSION: u32 = 1;
pub const SIDECAR_EXTENSION: &str = ".meta.toml";

/// Width of the `YYYYMMDD-HHMMSS` timestamp embedded in snapshot ids.
const ID_LEN: usize = 15;

/// What caused a snapshot to be taken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TriggerKind {
    Manual,
    Pacman,
    SystemdBoot,
    SystemdPeriodic,
    Restore,
    #[default]
    Unknown,
}

/// Pacman-specific trigger context, captured from the PreTransaction hook.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacmanTrigger {
    /// Package target list from stdin (requires `NeedsTargets` in the hook).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
}

/// Systemd-unit trigger context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemdTrigger {
    /// The systemd unit that fired the snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// Restore-specific trigger context.  Set on pre-restore safety snapshots
/// created via `revenantctl restore --save-current`; carries the id of the
/// snapshot that was about to be restored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreTrigger {
    /// Id of the snapshot being restored when this pre-restore snapshot was taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// Structured trigger information for a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger {
    pub kind: TriggerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pacman: Option<PacmanTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub systemd: Option<SystemdTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<RestoreTrigger>,
}

impl Default for Trigger {
    fn default() -> Self {
        Self {
            kind: TriggerKind::Unknown,
            pacman: None,
            systemd: None,
            restore: None,
        }
    }
}

impl Trigger {
    #[must_use]
    pub fn manual() -> Self {
        Self {
            kind: TriggerKind::Manual,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn pacman(targets: Vec<String>) -> Self {
        Self {
            kind: TriggerKind::Pacman,
            pacman: Some(PacmanTrigger { targets }),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn systemd(kind: TriggerKind, unit: Option<String>) -> Self {
        debug_assert!(matches!(
            kind,
            TriggerKind::SystemdBoot | TriggerKind::SystemdPeriodic
        ));
        Self {
            kind,
            systemd: Some(SystemdTrigger { unit }),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn restore(target: String) -> Self {
        Self {
            kind: TriggerKind::Restore,
            restore: Some(RestoreTrigger {
                target: Some(target),
            }),
            ..Self::default()
        }
    }
}

/// Full metadata record written alongside a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    /// Schema version; bumped when the on-disk format changes incompatibly.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Free-form description supplied by the user via `--message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Wall-clock creation time with fixed offset. Distinct from the UTC
    /// timestamp embedded in the snapshot id, so the sidecar is readable on
    /// its own. Stored as `FixedOffset` so the value is frozen at write
    /// time and does not shift when the reader's timezone changes.
    pub created_at: DateTime<FixedOffset>,
    #[serde(default)]
    pub trigger: Trigger,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl SnapshotMetadata {
    /// Build a metadata record for a snapshot being created now.
    #[must_use]
    pub fn new(message: Option<String>, trigger: Trigger) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            message,
            created_at: Local::now().fixed_offset(),
            trigger,
        }
    }
}

/// Path of the sidecar file for a given (strain, id). The sidecar lives
/// inside the snapshot directory itself, independent of any particular
/// snapshot subvolume name.
#[must_use]
pub fn sidecar_path(snap_dir: &Path, strain: &str, id: &str) -> PathBuf {
    snap_dir.join(format!("{strain}-{id}{SIDECAR_EXTENSION}"))
}

/// Parse a sidecar file name of the form `<strain>-YYYYMMDD-HHMMSS.meta.toml`
/// into its `(strain, id)` components.
///
/// Returns `None` if the file name does not match the expected shape.
/// Strain names are restricted to `[a-zA-Z0-9_]` (no hyphens), mirroring
/// the constraint used by snapshot subvolume names.
#[must_use]
pub fn parse_sidecar_name(name: &str) -> Option<(String, String)> {
    let stem = name.strip_suffix(SIDECAR_EXTENSION)?;
    // Minimum: 1 strain char + '-' + 15-char timestamp = 17 chars.
    if stem.len() < ID_LEN + 2 {
        return None;
    }
    let ts_start = stem.len() - ID_LEN;
    let ts = &stem[ts_start..];
    // Validate timestamp shape and parseability.
    if ts.as_bytes().get(8) != Some(&b'-')
        || NaiveDateTime::parse_from_str(ts, "%Y%m%d-%H%M%S").is_err()
    {
        return None;
    }
    if stem.as_bytes()[ts_start - 1] != b'-' {
        return None;
    }
    let strain = &stem[..ts_start - 1];
    if strain.is_empty()
        || !strain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    Some((strain.to_string(), ts.to_string()))
}

/// A sidecar file whose companion snapshot subvolume no longer exists.
#[derive(Debug, Clone)]
pub struct OrphanedSidecar {
    pub path: PathBuf,
    pub name: String,
}

/// Find sidecar files in `snap_dir` whose matching snapshot subvolume
/// (any subvolume whose name ends with `-<strain>-<id>`) is absent.
///
/// Shared backbone for `check::find_orphaned_sidecars` and
/// `cleanup::purge_orphaned_sidecars` so their matching rules stay in
/// lockstep. Returns an empty vector when `snap_dir` does not exist.
/// Results are sorted by file name for stable output.
pub fn find_orphaned_sidecars(
    snap_dir: &Path,
    backend: &dyn FileSystemBackend,
) -> Result<Vec<OrphanedSidecar>> {
    if !subvol_exists(backend, snap_dir) {
        return Ok(Vec::new());
    }
    let entries = match std::fs::read_dir(snap_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(RevenantError::io(snap_dir, e)),
    };

    // Snapshot subvolumes all live under snap_dir; listing them once is
    // cheaper than an individual existence check per sidecar.
    let subvol_names: HashSet<String> = backend
        .list_subvolumes(snap_dir)?
        .iter()
        .filter_map(|s| s.path.file_name()?.to_str().map(ToString::to_string))
        .collect();

    let mut orphans = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((strain, id)) = parse_sidecar_name(name) else {
            continue;
        };
        let suffix = format!("-{strain}-{id}");
        if subvol_names.iter().any(|n| n.ends_with(&suffix)) {
            continue;
        }
        orphans.push(OrphanedSidecar {
            path: path.clone(),
            name: name.to_string(),
        });
    }

    orphans.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(orphans)
}

/// Read a sidecar, returning `Ok(None)` if the file is absent.
///
/// Malformed TOML is an error — we prefer a loud failure to silently dropping
/// metadata the user asked for. A `schema_version` higher than what this
/// build understands is accepted but logged, because serde's default
/// behaviour is to ignore unknown fields and the caller should at least
/// know it is looking at a forward-compatible read.
pub fn read(path: &Path) -> Result<Option<SnapshotMetadata>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let meta: SnapshotMetadata = toml::from_str(&text).map_err(|e| {
                RevenantError::Other(format!(
                    "failed to parse snapshot metadata at {}: {e}",
                    path.display()
                ))
            })?;
            if meta.schema_version > SCHEMA_VERSION {
                tracing::warn!(
                    "sidecar {} has schema_version {} (this build supports up to {}); \
                     unknown fields will be ignored",
                    path.display(),
                    meta.schema_version,
                    SCHEMA_VERSION
                );
            }
            Ok(Some(meta))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RevenantError::io(path, e)),
    }
}

/// Write a sidecar atomically: serialize to a `.tmp` file in the same
/// directory, fsync the file contents, then rename into place.
///
/// The fsync before rename ensures that if the system crashes after the
/// rename becomes durable the file contents are durable too — a plain
/// `fs::write` + rename on ext4/data=ordered can in theory leave the
/// renamed entry pointing at a zero-byte file.
pub fn write(path: &Path, meta: &SnapshotMetadata) -> Result<()> {
    let text = toml::to_string_pretty(meta).map_err(|e| {
        RevenantError::Other(format!(
            "failed to serialize snapshot metadata for {}: {e}",
            path.display()
        ))
    })?;
    let tmp_path = {
        let mut s: std::ffi::OsString = path.as_os_str().into();
        s.push(".tmp");
        PathBuf::from(s)
    };
    {
        let mut f = File::create(&tmp_path).map_err(|e| RevenantError::io(&tmp_path, e))?;
        f.write_all(text.as_bytes())
            .map_err(|e| RevenantError::io(&tmp_path, e))?;
        f.sync_all().map_err(|e| RevenantError::io(&tmp_path, e))?;
    }
    std::fs::rename(&tmp_path, path).map_err(|e| RevenantError::io(path, e))?;
    Ok(())
}

/// Remove a sidecar if it exists. Missing file is not an error.
pub fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(RevenantError::io(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir() -> PathBuf {
        let name = format!("revenant-meta-{}", uuid::Uuid::new_v4());
        let p = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn sidecar_path_strain_keyed() {
        let p = sidecar_path(Path::new("/snap"), "default", "20260316-143022");
        assert_eq!(p, PathBuf::from("/snap/default-20260316-143022.meta.toml"));
    }

    #[test]
    fn parse_sidecar_name_accepts_valid() {
        let (strain, id) = parse_sidecar_name("default-20260316-143022.meta.toml").unwrap();
        assert_eq!(strain, "default");
        assert_eq!(id, "20260316-143022");
    }

    #[test]
    fn parse_sidecar_name_accepts_underscore_strain() {
        let (strain, id) = parse_sidecar_name("my_strain-20260316-143022.meta.toml").unwrap();
        assert_eq!(strain, "my_strain");
        assert_eq!(id, "20260316-143022");
    }

    #[test]
    fn parse_sidecar_name_rejects_bad_shapes() {
        assert!(parse_sidecar_name("noext").is_none());
        assert!(parse_sidecar_name("default-bogus.meta.toml").is_none());
        assert!(parse_sidecar_name("default-99999999-999999.meta.toml").is_none());
        assert!(parse_sidecar_name("-20260316-143022.meta.toml").is_none());
        // Strain with a hyphen is not valid.
        assert!(parse_sidecar_name("a-b-20260316-143022.meta.toml").is_none());
    }

    #[test]
    fn read_missing_returns_none() {
        let dir = tmpdir();
        let missing = dir.join("does-not-exist.meta.toml");
        assert!(read(&missing).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trip_manual_with_message() {
        let dir = tmpdir();
        let path = dir.join("x.meta.toml");
        let meta =
            SnapshotMetadata::new(Some("pre-upgrade sanity check".into()), Trigger::manual());
        write(&path, &meta).unwrap();
        let loaded = read(&path).unwrap().unwrap();
        assert_eq!(loaded.message.as_deref(), Some("pre-upgrade sanity check"));
        assert_eq!(loaded.trigger.kind, TriggerKind::Manual);
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trip_pacman_with_targets() {
        let dir = tmpdir();
        let path = dir.join("p.meta.toml");
        let meta =
            SnapshotMetadata::new(None, Trigger::pacman(vec!["linux".into(), "mesa".into()]));
        write(&path, &meta).unwrap();
        let loaded = read(&path).unwrap().unwrap();
        assert_eq!(loaded.trigger.kind, TriggerKind::Pacman);
        let pac = loaded.trigger.pacman.as_ref().unwrap();
        assert_eq!(pac.targets, vec!["linux".to_string(), "mesa".to_string()]);
        assert!(loaded.trigger.systemd.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trip_systemd_boot() {
        let dir = tmpdir();
        let path = dir.join("s.meta.toml");
        let meta = SnapshotMetadata::new(
            None,
            Trigger::systemd(
                TriggerKind::SystemdBoot,
                Some("revenant-boot.service".into()),
            ),
        );
        write(&path, &meta).unwrap();
        let loaded = read(&path).unwrap().unwrap();
        assert_eq!(loaded.trigger.kind, TriggerKind::SystemdBoot);
        assert_eq!(
            loaded
                .trigger
                .systemd
                .as_ref()
                .and_then(|s| s.unit.as_deref()),
            Some("revenant-boot.service")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trip_restore_with_target() {
        let dir = tmpdir();
        let path = dir.join("r.meta.toml");
        let meta = SnapshotMetadata::new(None, Trigger::restore("20260420-230031".into()));
        write(&path, &meta).unwrap();
        let loaded = read(&path).unwrap().unwrap();
        assert_eq!(loaded.trigger.kind, TriggerKind::Restore);
        assert_eq!(
            loaded
                .trigger
                .restore
                .as_ref()
                .and_then(|r| r.target.as_deref()),
            Some("20260420-230031")
        );
        assert!(loaded.trigger.pacman.is_none());
        assert!(loaded.trigger.systemd.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_rejects_malformed_toml() {
        let dir = tmpdir();
        let path = dir.join("bad.meta.toml");
        std::fs::write(&path, "this is = not [valid toml").unwrap();
        assert!(read(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_tolerates_unknown_fields() {
        // Forward-compat: a newer writer added a field; older reader must
        // still be able to load what it understands.
        let dir = tmpdir();
        let path = dir.join("fwd.meta.toml");
        let text = r#"
schema_version = 99
created_at = "2026-04-14T14:05:01+02:00"
message = "hello"
future_field = "ignored"

[trigger]
kind = "manual"
future_nested = 1
"#;
        std::fs::write(&path, text).unwrap();
        let loaded = read(&path).unwrap().unwrap();
        assert_eq!(loaded.message.as_deref(), Some("hello"));
        assert_eq!(loaded.trigger.kind, TriggerKind::Manual);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_missing_is_ok() {
        let dir = tmpdir();
        let missing = dir.join("gone.meta.toml");
        remove(&missing).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_existing_deletes() {
        let dir = tmpdir();
        let path = dir.join("r.meta.toml");
        write(&path, &SnapshotMetadata::new(None, Trigger::manual())).unwrap();
        assert!(path.exists());
        remove(&path).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_orphans_uses_strain_id_matching() {
        use crate::backend::mock::MockBackend;

        let dir = tmpdir();
        let mock = MockBackend::new();
        mock.seed_subvolume(dir.clone());
        // Subvol present with any anchor — sidecar must be considered paired.
        mock.seed_subvolume(dir.join("@home-default-20260316-143022"));

        let paired = dir.join("default-20260316-143022.meta.toml");
        std::fs::write(
            &paired,
            "schema_version = 1\ncreated_at = \"2026-04-14T14:05:01+02:00\"\n[trigger]\nkind = \"manual\"\n",
        )
        .unwrap();

        // Orphan: no subvol with the (strain=default, id=20260101-000000) pair.
        let orphan = dir.join("default-20260101-000000.meta.toml");
        std::fs::write(
            &orphan,
            "schema_version = 1\ncreated_at = \"2026-04-14T14:05:01+02:00\"\n[trigger]\nkind = \"manual\"\n",
        )
        .unwrap();

        let found = find_orphaned_sidecars(&dir, &mock).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "default-20260101-000000.meta.toml");

        std::fs::remove_dir_all(&dir).ok();
    }
}
