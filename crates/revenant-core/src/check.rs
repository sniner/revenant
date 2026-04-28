//! System health checks for revenant.
//!
//! Each check returns a list of [`Finding`]s describing observed issues. Findings
//! carry a severity, a stable check identifier (so output can be filtered or
//! suppressed), a human-readable message and optionally a hint with a suggested
//! action.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use crate::backend::{FileSystemBackend, subvol_exists};
use crate::config::{Config, DELETE_STRAIN};
use crate::error::Result;
use crate::metadata;
use crate::snapshot::{self, SnapshotId};

/// Severity of a check finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Warning => "WARN",
            Severity::Error => "ERROR",
        }
    }
}

/// A single observation produced by a check.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub severity: Severity,
    /// Stable identifier for the check that produced this finding.
    pub check: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Finding {
    #[must_use]
    pub fn info(check: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Info,
            check,
            message: message.into(),
            hint: None,
        }
    }

    #[must_use]
    pub fn warning(check: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            check,
            message: message.into(),
            hint: None,
        }
    }

    #[must_use]
    pub fn error(check: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            check,
            message: message.into(),
            hint: None,
        }
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Result of parsing a revenant-style snapshot subvolume name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSnapshotName {
    pub subvol: String,
    pub strain: String,
    pub id: SnapshotId,
}

/// Try to parse a subvolume entry name as `{subvol}-{strain}-{id}`,
/// where `id` is `YYYYMMDD-HHMMSS-NNN` (current) or `YYYYMMDD-HHMMSS`
/// (legacy).
///
/// Returns `None` if the name does not look like a revenant snapshot. Strain
/// names are restricted to `[a-zA-Z0-9_]` (no hyphens), which makes the split
/// between `subvol` and `strain` unambiguous: the rightmost hyphen before the
/// timestamp separates them.
#[must_use]
pub fn parse_snapshot_name(name: &str) -> Option<ParsedSnapshotName> {
    let (id, ts_start) = SnapshotId::extract_trailing(name)?;
    let prefix = &name[..ts_start - 1]; // "{subvol}-{strain}"
    let last_dash = prefix.rfind('-')?;
    let subvol = &prefix[..last_dash];
    let strain = &prefix[last_dash + 1..];
    if subvol.is_empty() || strain.is_empty() {
        return None;
    }
    if !strain
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    Some(ParsedSnapshotName {
        subvol: subvol.to_string(),
        strain: strain.to_string(),
        id,
    })
}

/// Check whether the configuration file exists and parses successfully.
///
/// Unlike other checks, this one does not require a loaded `Config` — it
/// operates on the raw filesystem path so it can produce a useful finding
/// before [`Config::load`] would fail.
#[must_use]
pub fn check_config_file(path: &Path) -> Vec<Finding> {
    if !path.exists() {
        return vec![
            Finding::error(
                "config-missing",
                format!("configuration file not found: {}", path.display()),
            )
            .with_hint("run `revenantctl init` to generate a configuration file"),
        ];
    }
    match Config::load(path) {
        Ok(_) => Vec::new(),
        Err(e) => vec![
            Finding::error(
                "config-invalid",
                format!("failed to load {}: {e}", path.display()),
            )
            .with_hint(
                "fix the configuration, or remove the file and re-run `revenantctl init` to regenerate it from system detection",
            ),
        ],
    }
}

/// Find subvolumes in the snapshot directory whose names look like revenant
/// snapshots but do not belong to any configured strain.
///
/// This detects two situations:
/// - leftover snapshots from a strain that was removed from the config
/// - typos / external snapshots that happen to share the naming scheme
///
/// Subvolumes belonging to the reserved [`DELETE_STRAIN`] marker are ignored.
pub fn find_orphaned_snapshots(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<Vec<Finding>> {
    let snap_dir = toplevel.join(&config.sys.snapshot_subvol);
    if !subvol_exists(backend, &snap_dir) {
        return Ok(Vec::new());
    }

    let subvols = backend.list_subvolumes(&snap_dir)?;
    let entries: Vec<String> = subvols
        .iter()
        .filter_map(|s| s.path.file_name()?.to_str().map(ToString::to_string))
        .collect();

    // Build the set of names that are claimed by configured strains.
    let claimed_snapshots = snapshot::discover_snapshots(config, backend, toplevel)?;
    let mut claimed: HashSet<String> = HashSet::new();
    for snap in &claimed_snapshots {
        for subvol in &snap.subvolumes {
            claimed.insert(snap.id.snapshot_name(subvol, &snap.strain));
        }
    }

    let mut findings = Vec::new();
    for entry in &entries {
        if claimed.contains(entry) {
            continue;
        }
        let Some(parsed) = parse_snapshot_name(entry) else {
            continue;
        };
        // Lifecycle marker, not an orphan.
        if parsed.strain == DELETE_STRAIN {
            continue;
        }

        let known_strain = config.strain.contains_key(&parsed.strain);
        let known_subvol = config
            .strain
            .values()
            .any(|sc| sc.subvolumes.contains(&parsed.subvol))
            || (config.sys.efi.enabled && parsed.subvol == config.sys.efi.staging_subvol);

        let detail = match (known_strain, known_subvol) {
            (false, _) => format!("unknown strain '{}'", parsed.strain),
            (true, false) => format!(
                "strain '{}' is defined but does not include subvolume '{}'",
                parsed.strain, parsed.subvol
            ),
            (true, true) => "strain definition no longer covers this snapshot".to_string(),
        };

        let hint = if known_strain {
            format!(
                "add '{}' to strain.{}.subvolumes or remove the snapshot manually",
                parsed.subvol, parsed.strain
            )
        } else {
            format!(
                "define strain.{} in the config and run `revenantctl delete {}@`, or remove manually",
                parsed.strain, parsed.strain
            )
        };

        findings.push(
            Finding::warning("orphaned-snapshot", format!("{entry}  ({detail})")).with_hint(hint),
        );
    }

    findings.sort_by(|a, b| a.message.cmp(&b.message));
    Ok(findings)
}

/// Find sidecar metadata files in the snapshot directory whose matching
/// snapshot subvolume is gone. Delegates to
/// [`metadata::find_orphaned_sidecars`] and wraps each hit in a
/// [`Finding`].
pub fn find_orphaned_sidecars(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<Vec<Finding>> {
    let snap_dir = toplevel.join(&config.sys.snapshot_subvol);
    let orphans = metadata::find_orphaned_sidecars(&snap_dir, backend)?;
    let findings = orphans
        .into_iter()
        .map(|o| {
            Finding::warning(
                "orphaned-sidecar",
                format!("{}  (no matching snapshot subvolume)", o.name),
            )
            .with_hint("run `revenantctl cleanup` to remove orphaned sidecar metadata")
        })
        .collect();
    Ok(findings)
}

/// Check configured snapshot sources for nested subvolumes the user
/// should be aware of.
///
/// Btrfs snapshots stop at subvolume boundaries, so the *contents* of a
/// nested subvolume are not versioned alongside its parent. Revenant
/// still preserves the nested subvolume itself across a restore by
/// re-attaching it to the rolled-back parent at its *current* state, so
/// no data is lost — but a rollback will not revert its contents.
/// Reporting them here lets users decide whether that semantic matches
/// their intent.
#[must_use]
pub fn find_nested_subvolumes(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Vec<Finding> {
    let all_subvols: BTreeSet<&str> = config
        .strain
        .values()
        .flat_map(|sc| sc.subvolumes.iter().map(String::as_str))
        .collect();

    let mut findings = Vec::new();
    for subvol in &all_subvols {
        let subvol_path = toplevel.join(subvol);
        let Ok(nested) = backend.find_nested_subvolumes(&subvol_path) else {
            continue;
        };
        if nested.is_empty() {
            continue;
        }
        let mut lines = format!(
            "'{subvol}' contains nested subvolumes whose contents are not versioned by snapshots:"
        );
        for n in &nested {
            let rel = n.strip_prefix(&subvol_path).unwrap_or(n);
            let _ = write!(lines, "\n    {subvol}/{}", rel.display());
        }
        findings.push(Finding::info("nested-subvolumes", lines).with_hint(
            "revenant re-attaches these across a restore at their current state — \
             a rollback will not revert their contents",
        ));
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    fn config_one_strain(subvols: &[&str]) -> Config {
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

    fn seed_in_snap_dir(mock: &MockBackend, config: &Config, toplevel: &Path, name: &str) {
        let snap_dir = toplevel.join(&config.sys.snapshot_subvol);
        mock.seed_subvolume(snap_dir.join(name));
    }

    // ----- find_orphaned_snapshots -----

    #[test]
    fn orphans_empty_when_snap_dir_missing() {
        let config = config_one_strain(&["@"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        // No snap dir seeded.
        let findings = find_orphaned_snapshots(&config, &mock, toplevel).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn orphans_empty_when_all_known() {
        let config = config_one_strain(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        seed_in_snap_dir(&mock, &config, toplevel, "@-default-20260316-143022");
        seed_in_snap_dir(&mock, &config, toplevel, "@-default-20260317-143022");

        let findings = find_orphaned_snapshots(&config, &mock, toplevel).unwrap();
        assert!(
            findings.is_empty(),
            "expected no findings, got: {findings:?}"
        );
    }

    #[test]
    fn orphans_unknown_strain_flagged() {
        let config = config_one_strain(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        seed_in_snap_dir(&mock, &config, toplevel, "@-removed-20260316-143022");

        let findings = find_orphaned_snapshots(&config, &mock, toplevel).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, "orphaned-snapshot");
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(findings[0].message.contains("unknown strain 'removed'"));
    }

    #[test]
    fn orphans_known_strain_unknown_subvol_flagged() {
        // Strain `default` is configured but only over @, not @home.
        let config = config_one_strain(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        seed_in_snap_dir(&mock, &config, toplevel, "@home-default-20260316-143022");

        let findings = find_orphaned_snapshots(&config, &mock, toplevel).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("strain 'default'")
                && findings[0].message.contains("'@home'"),
            "unexpected message: {}",
            findings[0].message
        );
    }

    #[test]
    fn orphans_ignore_delete_marker() {
        let config = config_one_strain(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        seed_in_snap_dir(&mock, &config, toplevel, "@-DELETE-20260316-143022");

        let findings = find_orphaned_snapshots(&config, &mock, toplevel).unwrap();
        assert!(findings.is_empty(), "DELETE marker should not be flagged");
    }

    #[test]
    fn orphans_ignore_unrelated_entries() {
        let config = config_one_strain(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        // Names that don't parse as snapshot names should be ignored.
        seed_in_snap_dir(&mock, &config, toplevel, "random-directory");
        seed_in_snap_dir(&mock, &config, toplevel, "@-default-bogus");
        seed_in_snap_dir(&mock, &config, toplevel, "definitely-not-a-snapshot");

        let findings = find_orphaned_snapshots(&config, &mock, toplevel).unwrap();
        assert!(
            findings.is_empty(),
            "non-parseable names should be ignored: {findings:?}"
        );
    }

    #[test]
    fn orphans_findings_sorted() {
        let config = config_one_strain(&["@"]);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);
        seed_in_snap_dir(&mock, &config, toplevel, "@-zremoved-20260316-143022");
        seed_in_snap_dir(&mock, &config, toplevel, "@-aremoved-20260316-143022");
        seed_in_snap_dir(&mock, &config, toplevel, "@-mremoved-20260316-143022");

        let findings = find_orphaned_snapshots(&config, &mock, toplevel).unwrap();
        assert_eq!(findings.len(), 3);
        // Sorted by message — entry name appears first in each message.
        assert!(findings[0].message.starts_with("@-aremoved-"));
        assert!(findings[1].message.starts_with("@-mremoved-"));
        assert!(findings[2].message.starts_with("@-zremoved-"));
    }

    // ----- find_orphaned_sidecars -----

    fn tmpdir() -> std::path::PathBuf {
        let name = format!("revenant-check-{}", uuid::Uuid::new_v4());
        let p = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_sidecar(snap_dir: &Path, stem: &str) -> std::path::PathBuf {
        let p = snap_dir.join(format!("{stem}{}", metadata::SIDECAR_EXTENSION));
        std::fs::write(&p, "schema_version = 1\ncreated_at = \"2026-04-14T14:05:01+02:00\"\ntrigger = \"manual\"\n").unwrap();
        p
    }

    #[test]
    fn sidecars_empty_when_snap_dir_missing() {
        let config = config_one_strain(&["@"]);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        let findings = find_orphaned_sidecars(&config, &mock, toplevel).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn sidecars_ignored_when_subvol_present() {
        let config = config_one_strain(&["@"]);
        let dir = tmpdir();
        let mock = setup_mock(&config, &dir);
        let snap_dir = dir.join(&config.sys.snapshot_subvol);
        std::fs::create_dir_all(&snap_dir).unwrap();
        seed_in_snap_dir(&mock, &config, &dir, "@-default-20260316-143022");
        write_sidecar(&snap_dir, "default-20260316-143022");

        let findings = find_orphaned_sidecars(&config, &mock, &dir).unwrap();
        assert!(findings.is_empty(), "got: {findings:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sidecar_without_subvol_flagged() {
        let config = config_one_strain(&["@"]);
        let dir = tmpdir();
        let mock = setup_mock(&config, &dir);
        let snap_dir = dir.join(&config.sys.snapshot_subvol);
        std::fs::create_dir_all(&snap_dir).unwrap();
        // Sidecar whose subvol was deleted out from under it.
        write_sidecar(&snap_dir, "default-20260316-143022");

        let findings = find_orphaned_sidecars(&config, &mock, &dir).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, "orphaned-sidecar");
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(
            findings[0]
                .message
                .starts_with("default-20260316-143022.meta.toml"),
            "unexpected message: {}",
            findings[0].message
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sidecars_ignore_non_sidecar_files() {
        let config = config_one_strain(&["@"]);
        let dir = tmpdir();
        let mock = setup_mock(&config, &dir);
        let snap_dir = dir.join(&config.sys.snapshot_subvol);
        std::fs::create_dir_all(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("README"), "hello").unwrap();
        std::fs::write(snap_dir.join("x.toml"), "y = 1").unwrap();

        let findings = find_orphaned_sidecars(&config, &mock, &dir).unwrap();
        assert!(findings.is_empty(), "got: {findings:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ----- parse_snapshot_name -----

    #[test]
    fn parse_simple_name_legacy_id() {
        let p = parse_snapshot_name("@-default-20260316-143022").unwrap();
        assert_eq!(p.subvol, "@");
        assert_eq!(p.strain, "default");
        assert_eq!(p.id.as_str(), "20260316-143022");
    }

    #[test]
    fn parse_simple_name_current_id() {
        let p = parse_snapshot_name("@-default-20260316-143022-456").unwrap();
        assert_eq!(p.subvol, "@");
        assert_eq!(p.strain, "default");
        assert_eq!(p.id.as_str(), "20260316-143022-456");
    }

    #[test]
    fn parse_subvol_with_hyphen() {
        let p = parse_snapshot_name("@var-log-default-20260316-143022").unwrap();
        assert_eq!(p.subvol, "@var-log");
        assert_eq!(p.strain, "default");
    }

    #[test]
    fn parse_strain_with_underscore() {
        let p = parse_snapshot_name("@-my_strain-20260316-143022").unwrap();
        assert_eq!(p.strain, "my_strain");
    }

    #[test]
    fn parse_rejects_non_snapshot() {
        assert!(parse_snapshot_name("random-directory").is_none());
        assert!(parse_snapshot_name("@-default").is_none());
        assert!(parse_snapshot_name("@-default-99999999-999999").is_none());
        // Missing subvol part:
        assert!(parse_snapshot_name("-default-20260316-143022").is_none());
        // Missing strain part:
        assert!(parse_snapshot_name("@--20260316-143022").is_none());
    }

    #[test]
    fn parse_rejects_strain_with_hyphen() {
        // "@a-b-c-20260316-143022" — rightmost split gives strain="c", subvol="@a-b"
        // which is valid; we cannot detect a hyphen-strain because it is
        // ambiguous with a hyphen-subvol. This is by design.
        let p = parse_snapshot_name("@a-b-c-20260316-143022").unwrap();
        assert_eq!(p.subvol, "@a-b");
        assert_eq!(p.strain, "c");
    }

    #[test]
    fn parse_delete_marker() {
        let p = parse_snapshot_name("@-DELETE-20260316-143022").unwrap();
        assert_eq!(p.strain, "DELETE");
    }

    #[test]
    fn finding_builders() {
        let f = Finding::warning("c", "msg").with_hint("do x");
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.check, "c");
        assert_eq!(f.message, "msg");
        assert_eq!(f.hint.as_deref(), Some("do x"));
    }
}
