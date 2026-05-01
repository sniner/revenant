use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::backend::{FileSystemBackend, subvol_exists};
use crate::config::{Config, DELETE_STRAIN, RetainConfig};
use crate::error::Result;
use crate::metadata;
use crate::retention::{KeepReason, select_to_keep, select_to_keep_explained};
use crate::snapshot::{self, SnapshotId, SnapshotInfo};

/// Set of base subvolume names whose `<base>-DELETE-<ts>` tombstones are
/// considered ours to enumerate or purge — the strain subvols plus the
/// EFI staging subvol when EFI sync is enabled. Tombstones with any
/// other prefix are ignored so a cleanup run never touches subvolumes
/// outside revenant's surface area.
fn collect_base_names(config: &Config) -> HashSet<&str> {
    let mut base_names: HashSet<&str> = HashSet::new();
    for sc in config.strain.values() {
        for sv in &sc.subvolumes {
            base_names.insert(sv.as_str());
        }
    }
    if config.sys.efi.enabled {
        base_names.insert(config.sys.efi.staging_subvol.as_str());
    }
    base_names
}

/// What the retention policy would do, without touching the filesystem.
///
/// Built by [`plan_retention`] and rendered by the CLI's `--dry-run` path.
/// The fields are serialisable so the same structure can be emitted as
/// JSON in a future `--json` mode.
#[derive(Debug, Clone, Serialize)]
pub struct RetentionPlan {
    /// One entry per configured strain, sorted by strain name for stable output.
    pub strains: Vec<StrainPlan>,
    /// Structured view of every tombstone (`<base>-DELETE-<ts>` subvol)
    /// at the top level, each annotated with whether the next live
    /// retention run would purge it now (`would_purge`) or carry it as
    /// an undo buffer until `expires_at`.  May be empty.
    #[serde(rename = "delete_markers")]
    pub tombstones: Vec<PlanTombstone>,
}

/// A tombstone — a parsed `<base>-DELETE-<ts>` subvolume.
///
/// Tombstones are the renamed pre-restore live subvolumes; they survive
/// a restore as the user's "previous state", until an explicit
/// `revenantctl cleanup` (or a subsequent restore) removes them.  A
/// dry-run plan carries them as a structured value rather than a raw
/// subvolume name so JSON consumers don't have to parse the
/// `base-DELETE-timestamp` convention themselves.
///
/// Externally (CLI strings, D-Bus method names, polkit actions) we keep
/// the term "delete marker" / "DELETE" — `Tombstone` is purely the
/// internal Rust name because "delete" is too generic for this very
/// specific concept.
#[derive(Debug, Clone, Serialize)]
pub struct Tombstone {
    /// The live subvol this tombstone was renamed from, e.g. `"@"`.
    pub base_subvol: String,
    /// Timestamp the tombstone was created (at the moment of the restore).
    pub id: SnapshotId,
    /// Full subvolume name, e.g. `"@-DELETE-20260411-080055"`.
    pub name: String,
    /// Wall-clock at which this tombstone ages out and becomes eligible
    /// for auto-purge by the next retention run — `created_at +
    /// sys.tombstone_max_age_days`.  `None` when auto-expiry is
    /// disabled (`tombstone_max_age_days = 0`) or the timestamp suffix
    /// could not be parsed.
    pub expires_at: Option<DateTime<Utc>>,
}

/// A [`Tombstone`] decorated with the live cleanup decision derived
/// from a specific "now": `would_purge = true` iff `expires_at` is set
/// and already in the past.  Used inside [`RetentionPlan`] so JSON
/// consumers and the CLI text renderer can split the dry-run report
/// into "pending purge" vs. "kept (within undo window)" without
/// needing to know the comparison instant themselves.
#[derive(Debug, Clone, Serialize)]
pub struct PlanTombstone {
    #[serde(flatten)]
    pub tombstone: Tombstone,
    /// Would the next live retention run purge this tombstone right now?
    pub would_purge: bool,
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
/// tombstone — typically because a previous `restore` was interrupted
/// between the rename of the live subvolume and the nested-subvolume
/// re-attach step (e.g. power loss after the rename but before the
/// loop that moves the nested subvols into the freshly restored
/// subvolume completed).
///
/// For each tombstone that contains nested subvolumes, attempt to move
/// those nested subvolumes back into the corresponding live `<base>`
/// subvolume at their original relative paths.  Per-entry failures
/// (live subvol missing, destination already exists as a subvolume,
/// rename failed) are logged as warnings — the tombstone is left in
/// place so the user or a later run can investigate, and the function
/// never errors out so the surrounding write command can still proceed.
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
    // Same base-name set as `purge_all_tombstones`: only tombstones
    // whose prefix matches a known strain subvol (or the EFI staging
    // subvol) are considered ours to touch.
    let base_names = collect_base_names(config);

    let all_subvols = backend.list_subvolumes(toplevel)?;
    let mut recovered = 0usize;

    for entry in all_subvols {
        let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Find which base this tombstone belongs to.  We need the
        // base name to figure out which live subvol the orphans should
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
                "tombstone {} contains {} nested subvolume(s) but live subvol {} does not exist; leaving tombstone in place for manual review",
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
                    "skipping recovery of {} → {}: destination already exists as a subvolume; leaving in tombstone for manual review",
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

/// Delete all tombstones (`<base>-DELETE-<ts>` subvols) across all
/// configured subvol names.
///
/// Bulk variant of the auto-expiry pass — kept for tests and for
/// future tooling that needs a "wipe every undo buffer" hammer.  The
/// `cleanup` command no longer calls this directly; it goes through
/// `purge_expired_tombstones`, which honours `tombstone_max_age_days`
/// and leaves recent undo buffers in place.  `create_snapshot` also
/// does not call this — tombstones are the volatile undo buffer for
/// the previous live state after a restore, and purging them on every
/// boot-time snapshot would defeat that purpose.  Failures per entry
/// are logged as warnings; the function always succeeds so that a
/// mounted subvolume (live root before reboot) does not block normal
/// operation.
pub fn purge_all_tombstones(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<Vec<String>> {
    let base_names = collect_base_names(config);

    let all_subvols = backend.list_subvolumes(toplevel)?;
    let mut removed = Vec::new();

    for entry in all_subvols {
        if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
            let matched = base_names
                .iter()
                .any(|base| name.starts_with(&format!("{base}-{DELETE_STRAIN}-")));
            if matched {
                tracing::info!("cleanup: purging tombstone '{name}'");
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

/// Purge tombstones older than `config.sys.tombstone_max_age_days`.
///
/// Used as the auto-cleanup pass: an explicit `cleanup` (or any retention
/// run) drops tombstones that have aged out, while keeping recent ones
/// available as undo buffers. `tombstone_max_age_days = 0` disables
/// auto-expiry entirely and the function is a no-op.
///
/// Tombstones whose timestamp suffix does not parse as a snapshot id
/// are kept — we have no way to age them, and a manual review is the
/// safer outcome.
///
/// Per-entry deletion failures are logged and skipped; the function
/// only fails for global problems (cannot list subvolumes, etc.).
pub fn purge_expired_tombstones(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    now: DateTime<Utc>,
) -> Result<Vec<String>> {
    let max_age_days = config.sys.tombstone_max_age_days;
    if max_age_days == 0 {
        return Ok(Vec::new());
    }
    let cutoff = now - Duration::days(max_age_days as i64);

    let tombstones = list_tombstones(config, backend, toplevel)?;
    let mut removed = Vec::new();
    for tombstone in tombstones {
        let Some(created) = tombstone.id.created_at() else {
            continue;
        };
        if created >= cutoff {
            continue;
        }
        let path = toplevel.join(&tombstone.name);
        tracing::info!(
            "cleanup: purging expired tombstone '{}' (older than {max_age_days}d)",
            tombstone.name
        );
        match delete_subvolume_recursive(backend, &path) {
            Ok(()) => removed.push(tombstone.name),
            Err(e) => tracing::warn!(
                "could not delete '{}': {e} (will be retried on next run)",
                tombstone.name
            ),
        }
    }
    Ok(removed)
}

/// Purge specific tombstones by subvolume name.
///
/// Used by the daemon's `PurgeDeleteMarkers` D-Bus method, where the GUI
/// hands a user-chosen subset of tombstones (after a "review" dialog)
/// to be removed.  Names that don't match any current tombstone are
/// skipped with a warning — they may have been removed by a concurrent
/// CLI `cleanup` between the GUI's listing and the user's confirmation.
///
/// Always recovers orphaned nested subvolumes first (same pattern as
/// the CLI's write commands), so an interrupted-restore tombstone is
/// not purged with live nested data still inside.  Per-entry deletion
/// failures are logged and skipped; the function only fails for global
/// problems (cannot list subvolumes, etc.).
///
/// Returns the names of tombstones that were actually removed.
pub fn purge_tombstones_by_name(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    names: &[String],
) -> Result<Vec<String>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let _ = recover_orphaned_nested_subvols(config, backend, toplevel)?;

    let wanted: HashSet<&str> = names.iter().map(String::as_str).collect();
    let tombstones = list_tombstones(config, backend, toplevel)?;
    let mut removed = Vec::new();

    for tombstone in tombstones {
        if !wanted.contains(tombstone.name.as_str()) {
            continue;
        }
        let path = toplevel.join(&tombstone.name);
        tracing::info!("purging tombstone '{}'", tombstone.name);
        match delete_subvolume_recursive(backend, &path) {
            Ok(()) => removed.push(tombstone.name),
            Err(e) => tracing::warn!(
                "could not delete '{}': {e} (will be retried on next run)",
                tombstone.name
            ),
        }
    }

    let removed_set: HashSet<&str> = removed.iter().map(String::as_str).collect();
    for name in names {
        if !removed_set.contains(name.as_str()) {
            tracing::warn!("tombstone '{name}' not found; skipping");
        }
    }

    Ok(removed)
}

/// Delete a subvolume and any subvolumes nested inside it.
///
/// Used by tombstone cleanup: when a restore renames the live `@`
/// subvolume to `@-DELETE-{ts}`, any nested subvolumes that lived inside
/// the original `@` (e.g. `@/var/lib/portables` on a stock Arch install)
/// move along with it. btrfs `SNAP_DESTROY` refuses to delete a subvolume
/// that still contains nested subvolumes (ENOTEMPTY), so we have to walk
/// the tombstone depth-first and clear it from the inside out.
///
/// This is intentionally only invoked for tombstones — never for live
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

/// List all tombstones at the top level that a subsequent
/// `purge_all_tombstones` would remove, as structured [`Tombstone`]s.
///
/// Used by `plan_retention` for the dry-run report and by the daemon's
/// `ListDeleteMarkers` D-Bus method.  Shares the same base-name set as
/// `purge_all_tombstones` / `recover_orphaned_nested_subvols` so the
/// three functions agree on what counts as "ours".
///
/// Tombstones whose timestamp suffix does not parse as a valid
/// [`SnapshotId`] are dropped with a warning — they may be leftovers
/// from a prior tool version or a hand-crafted name, and presenting
/// them as numbered structured data would be dishonest.  A real cleanup
/// run still picks them up because `purge_all_tombstones` only matches
/// the `<base>-DELETE-` prefix and does not parse the suffix.
pub fn list_tombstones(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<Vec<Tombstone>> {
    let base_names = collect_base_names(config);

    let max_age_days = config.sys.tombstone_max_age_days;

    let all_subvols = backend.list_subvolumes(toplevel)?;
    let mut tombstones = Vec::new();
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
            Ok(id) => {
                let expires_at = if max_age_days == 0 {
                    None
                } else {
                    id.created_at()
                        .map(|c| c + Duration::days(i64::from(max_age_days)))
                };
                tombstones.push(Tombstone {
                    base_subvol: base.to_string(),
                    id,
                    name: name.to_string(),
                    expires_at,
                });
            }
            Err(_) => {
                tracing::warn!(
                    "skipping tombstone '{name}' in dry-run report: timestamp suffix '{suffix}' is not a valid snapshot ID"
                );
            }
        }
    }
    tombstones.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tombstones)
}

/// Compute — without modifying anything on disk — what `apply_retention`
/// would do.  Returns a [`RetentionPlan`] that enumerates every snapshot
/// per configured strain with a keep/delete decision (plus the reasons
/// protecting a kept snapshot) and every tombstone, each annotated with
/// whether the next live retention run would purge it now or carry it
/// as an undo buffer until it ages out.
///
/// `force = true` mirrors `revenantctl cleanup --force`: every tombstone
/// is marked `would_purge` regardless of `expires_at`, so the dry-run
/// preview matches what a forced live run would actually do.
///
/// This is the engine behind `revenantctl cleanup --dry-run`.
///
/// Thin wrapper around [`plan_retention_with_now`] using `Utc::now()`.
/// Tests should call the `_with_now` variant with a fixed instant so
/// the tombstone-age cutoff is deterministic.
pub fn plan_retention(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    force: bool,
) -> Result<RetentionPlan> {
    plan_retention_with_now(config, backend, toplevel, Utc::now(), force)
}

/// Same as [`plan_retention`] but with an explicit "now" instant for
/// the `would_purge` decision on each tombstone.  Use this in tests.
pub fn plan_retention_with_now(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    now: DateTime<Utc>,
    force: bool,
) -> Result<RetentionPlan> {
    let tombstones = list_tombstones(config, backend, toplevel)?;
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

    // Match the live cleanup predicate: `purge_expired_tombstones`
    // purges when `created < now - max_age_days`, i.e. `expires_at < now`.
    // Under `force`, the live path swaps to `purge_all_tombstones`, so
    // every parsable tombstone is up for removal here too.
    let tombstones = tombstones
        .into_iter()
        .map(|t| {
            let would_purge = force || t.expires_at.is_some_and(|e| e < now);
            PlanTombstone {
                tombstone: t,
                would_purge,
            }
        })
        .collect();

    Ok(RetentionPlan {
        strains,
        tombstones,
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
/// (retention drops + purged tombstones) from orphaned sidecar file
/// removals so the UI can report each category individually.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CleanupSummary {
    /// Snapshot IDs and tombstone subvolume names that were removed.
    pub removed: Vec<String>,
    /// Filenames of orphaned sidecar metadata files that were removed.
    pub removed_sidecars: Vec<String>,
}

/// Apply per-strain retention policy: keep only the most recent `retain` snapshots per strain.
///
/// Also purges expired tombstones (older than
/// `sys.tombstone_max_age_days`) and orphaned sidecar metadata files.
/// Discovers snapshots on disk, then delegates to `apply_retention_to`.
///
/// Thin wrapper around [`apply_retention_with_now`] using `Utc::now()`.
/// Tests should call the `_with_now` variant with a fixed instant so
/// the tombstone-age cutoff is deterministic.
pub fn apply_retention(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<CleanupSummary> {
    apply_retention_with_now(config, backend, toplevel, Utc::now())
}

/// Same as [`apply_retention`] but with an explicit "now" instant for
/// the tombstone-age cutoff.  Use this in tests.
pub fn apply_retention_with_now(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    now: DateTime<Utc>,
) -> Result<CleanupSummary> {
    let mut removed = purge_expired_tombstones(config, backend, toplevel, now)?;
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

/// Forced variant of [`apply_retention`]: purge **every** tombstone
/// regardless of `tombstone_max_age_days`, but leave per-strain
/// retention rules untouched (they are policy, not a safety net).
///
/// This is the engine behind `revenantctl cleanup --force`.  Per-strain
/// retention and orphaned-sidecar cleanup behave the same as in
/// [`apply_retention`]; only the tombstone-age check is bypassed.
pub fn apply_retention_force(
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<CleanupSummary> {
    let mut removed = purge_all_tombstones(config, backend, toplevel)?;
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
    fn retention_purges_expired_tombstones() {
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Pre-existing tombstone, dated 2026-01-01.
        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));

        // Run retention "now" = 2026-04-01 → tombstone is 90 days old,
        // well past the 14-day default → must be purged.
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let summary = apply_retention_with_now(&config, &mock, toplevel, now).unwrap();
        assert!(
            summary
                .removed
                .contains(&"@-DELETE-20260101-120000".to_string())
        );
        assert!(!mock.contains("/top/@-DELETE-20260101-120000"));
    }

    #[test]
    fn retention_keeps_recent_tombstones() {
        // A tombstone newer than `tombstone_max_age_days` must survive
        // a retention run — that's the whole point of the cooldown.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Tombstone dated 2026-01-01; "now" two days later → 2 days
        // old, well within the 14-day default.
        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-03T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let summary = apply_retention_with_now(&config, &mock, toplevel, now).unwrap();
        assert!(
            !summary
                .removed
                .contains(&"@-DELETE-20260101-120000".to_string())
        );
        assert!(mock.contains("/top/@-DELETE-20260101-120000"));
    }

    #[test]
    fn tombstone_max_age_zero_disables_purge() {
        // `tombstone_max_age_days = 0` is the documented escape hatch
        // for users who want tombstones to stick around until they
        // explicitly purge them via the GUI dialog.
        let mut config = config_retain_last(&["@"], 5);
        config.sys.tombstone_max_age_days = 0;
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // A very old tombstone — would be purged at any non-zero
        // max-age, but must survive at zero.
        mock.seed_subvolume(toplevel.join("@-DELETE-20200101-120000"));
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let summary = apply_retention_with_now(&config, &mock, toplevel, now).unwrap();
        assert!(summary.removed.is_empty());
        assert!(mock.contains("/top/@-DELETE-20200101-120000"));
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
        std::fs::write(&kept_sidecar, "schema_version = 1\ncreated_at = \"2026-04-14T14:05:01+02:00\"\ntrigger = \"manual\"\n").unwrap();

        // Orphan — no subvolume with (strain=default, id=20260101-000000).
        let orphan_name = "default-20260101-000000.meta.toml";
        let orphan_path = snap_dir.join(orphan_name);
        std::fs::write(&orphan_path, "schema_version = 1\ncreated_at = \"2026-04-14T14:05:01+02:00\"\ntrigger = \"manual\"\n").unwrap();

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

        let plan = plan_retention(&config, &mock, toplevel, false).unwrap();
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

        // No tombstones in this scenario.
        assert!(plan.tombstones.is_empty());
    }

    #[test]
    fn plan_lists_empty_strains() {
        // A configured strain with no snapshots on disk should still appear
        // in the plan, with an empty entries list — the renderer prints
        // "(no snapshots)" for it.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        let plan = plan_retention(&config, &mock, toplevel, false).unwrap();
        assert_eq!(plan.strains.len(), 1);
        assert_eq!(plan.strains[0].strain, "default");
        assert!(plan.strains[0].entries.is_empty());
    }

    #[test]
    fn plan_reports_tombstones() {
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));
        // A foreign DELETE-ish name on an unknown base must NOT be listed.
        mock.seed_subvolume(toplevel.join("@home-DELETE-20260101-120000"));

        let plan = plan_retention(&config, &mock, toplevel, false).unwrap();
        assert_eq!(plan.tombstones.len(), 1);
        let pt = &plan.tombstones[0];
        assert_eq!(pt.tombstone.name, "@-DELETE-20260101-120000");
        assert_eq!(pt.tombstone.base_subvol, "@");
        assert_eq!(pt.tombstone.id.as_str(), "20260101-120000");
        // Dry run: tombstone still present on disk.
        assert!(mock.contains("/top/@-DELETE-20260101-120000"));
    }

    #[test]
    fn plan_drops_unparsable_tombstones() {
        // A `<base>-DELETE-<garbage>` name where <garbage> is not a
        // valid snapshot ID must be excluded from the dry-run report —
        // we don't have a trustworthy timestamp for JSON consumers.
        // (purge_all_tombstones would still remove it, since it only
        // matches the `<base>-DELETE-` prefix.)
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume(toplevel.join("@-DELETE-not-a-timestamp"));
        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));

        let plan = plan_retention(&config, &mock, toplevel, false).unwrap();
        // Only the well-formed one shows up.
        assert_eq!(plan.tombstones.len(), 1);
        assert_eq!(
            plan.tombstones[0].tombstone.name,
            "@-DELETE-20260101-120000"
        );
    }

    #[test]
    fn plan_splits_tombstones_by_age() {
        // Plan must mirror the live cleanup predicate: tombstones older
        // than `tombstone_max_age_days` get `would_purge = true`,
        // recent ones stay as undo buffers.  Regression test for the
        // 0.2.0 bug where the dry-run listed every tombstone as
        // "pending purge" while the live run only purged expired ones.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Two tombstones, default `tombstone_max_age_days = 14`.
        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000")); // very old
        mock.seed_subvolume(toplevel.join("@-DELETE-20260428-120000")); // 3 days old at "now" below

        let now = chrono::DateTime::parse_from_rfc3339("2026-05-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let plan = plan_retention_with_now(&config, &mock, toplevel, now, false).unwrap();
        assert_eq!(plan.tombstones.len(), 2);

        // Sorted alphabetically: the January tombstone first.
        assert_eq!(
            plan.tombstones[0].tombstone.name,
            "@-DELETE-20260101-120000"
        );
        assert!(plan.tombstones[0].would_purge);
        assert!(plan.tombstones[0].tombstone.expires_at.is_some());

        assert_eq!(
            plan.tombstones[1].tombstone.name,
            "@-DELETE-20260428-120000"
        );
        assert!(!plan.tombstones[1].would_purge);
        assert!(
            plan.tombstones[1]
                .tombstone
                .expires_at
                .is_some_and(|e| e > now)
        );
    }

    #[test]
    fn plan_max_age_zero_keeps_all_tombstones() {
        // `tombstone_max_age_days = 0` disables auto-expiry — `expires_at`
        // is `None` for all, `would_purge` is `false` for all.
        let mut config = config_retain_last(&["@"], 5);
        config.sys.tombstone_max_age_days = 0;
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume(toplevel.join("@-DELETE-20200101-120000"));

        let now = chrono::DateTime::parse_from_rfc3339("2026-05-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let plan = plan_retention_with_now(&config, &mock, toplevel, now, false).unwrap();
        assert_eq!(plan.tombstones.len(), 1);
        assert!(plan.tombstones[0].tombstone.expires_at.is_none());
        assert!(!plan.tombstones[0].would_purge);
    }

    #[test]
    fn plan_force_marks_all_tombstones_as_purge() {
        // `--force` overrides the age check: every parsable tombstone
        // shows up as `would_purge`, including the very recent one.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));
        mock.seed_subvolume(toplevel.join("@-DELETE-20260428-120000"));

        let now = chrono::DateTime::parse_from_rfc3339("2026-05-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let plan = plan_retention_with_now(&config, &mock, toplevel, now, true).unwrap();
        assert_eq!(plan.tombstones.len(), 2);
        assert!(plan.tombstones.iter().all(|t| t.would_purge));
    }

    #[test]
    fn apply_retention_force_purges_recent_tombstones() {
        // Live counterpart: `apply_retention_force` swaps the
        // age-gated `purge_expired_tombstones` for the unconditional
        // `purge_all_tombstones`, so even tombstones inside the undo
        // window are removed.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Tombstone two days old at our "now".  Without `--force` the
        // 14-day default would protect it.
        mock.seed_subvolume(toplevel.join("@-DELETE-20260428-120000"));

        let summary = apply_retention_force(&config, &mock, toplevel).unwrap();
        assert!(
            summary
                .removed
                .contains(&"@-DELETE-20260428-120000".to_string())
        );
        assert!(!mock.contains("/top/@-DELETE-20260428-120000"));
    }

    // ----- purge_all_tombstones -----

    #[test]
    fn purge_skips_unrelated_subvols() {
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Three things in toplevel that look superficially similar:
        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000")); // real tombstone
        mock.seed_subvolume(toplevel.join("@home-DELETE-20260101-120000")); // unknown base, ignored
        mock.seed_subvolume(toplevel.join("@-default-20260102-120000")); // not a tombstone

        let removed = purge_all_tombstones(&config, &mock, toplevel).unwrap();
        assert_eq!(removed, vec!["@-DELETE-20260101-120000".to_string()]);
        assert!(!mock.contains("/top/@-DELETE-20260101-120000"));
        // Other entries untouched
        assert!(mock.contains("/top/@home-DELETE-20260101-120000"));
        assert!(mock.contains("/top/@-default-20260102-120000"));
    }

    #[test]
    fn purge_recurses_into_tombstone_with_nested_subvols() {
        // Regression: a restore renames the live `@` to `@-DELETE-{ts}`,
        // which carries any nested subvolumes (e.g. var/lib/portables on
        // Arch) along with it. btrfs SNAP_DESTROY refuses to delete a
        // subvolume that still contains nested ones (ENOTEMPTY), so the
        // tombstone would otherwise stay around forever.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // The tombstone plus two nested subvols inside it (the mock's
        // delete_subvolume mirrors the real ENOTEMPTY behaviour, so
        // this test would fail without the recursive cleanup).
        let tombstone = toplevel.join("@-DELETE-20260101-120000");
        mock.seed_subvolume(tombstone.clone());
        mock.seed_subvolume(tombstone.join("var/lib/portables"));
        mock.seed_subvolume(tombstone.join("var/lib/machines"));

        let removed = purge_all_tombstones(&config, &mock, toplevel).unwrap();
        assert_eq!(removed, vec!["@-DELETE-20260101-120000".to_string()]);
        assert!(!mock.contains(&tombstone));
        assert!(!mock.contains(tombstone.join("var/lib/portables")));
        assert!(!mock.contains(tombstone.join("var/lib/machines")));
    }

    #[test]
    fn purge_handles_multiple_tombstones() {
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));
        mock.seed_subvolume(toplevel.join("@-DELETE-20260102-120000"));
        mock.seed_subvolume(toplevel.join("@-DELETE-20260103-120000"));

        let removed = purge_all_tombstones(&config, &mock, toplevel).unwrap();
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
        // restored one) plus a @-DELETE-{ts} tombstone carrying a
        // nested subvol that never got moved back.  The recovery hook
        // should put it back into @ at the same relative path.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        let tombstone = toplevel.join("@-DELETE-20260101-120000");
        mock.seed_subvolume(tombstone.clone());
        mock.seed_subvolume(tombstone.join("var/lib/portables"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        assert_eq!(recovered, 1);
        assert!(mock.contains("/top/@/var/lib/portables"));
        assert!(!mock.contains(tombstone.join("var/lib/portables")));
        // Tombstone itself stays — purge handles its removal once
        // it's empty of nested subvols.
        assert!(mock.contains(&tombstone));
    }

    #[test]
    fn recover_skips_tombstone_without_nested() {
        // The normal post-restore state: the tombstone is empty of
        // nested subvols.  Recovery should be a no-op.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        mock.seed_subvolume(toplevel.join("@-DELETE-20260101-120000"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        assert_eq!(recovered, 0);
    }

    #[test]
    fn recover_warns_when_live_subvol_missing() {
        // Pathological state: a tombstone carries nested subvols but
        // the live @ doesn't exist (e.g. crash between rename @ and
        // the create_writable_snapshot step).  Recovery should leave
        // the tombstone alone — moving the nested subvol into a
        // non-existent path would be wrong, and the system can't have
        // booted anyway.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = MockBackend::new();
        // Note: deliberately NOT seeding /top/@.
        mock.seed_subvolume(toplevel.join(&config.sys.snapshot_subvol));
        let tombstone = toplevel.join("@-DELETE-20260101-120000");
        mock.seed_subvolume(tombstone.clone());
        mock.seed_subvolume(tombstone.join("var/lib/portables"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        assert_eq!(recovered, 0);
        // Nothing was moved.
        assert!(mock.contains(tombstone.join("var/lib/portables")));
        assert!(!mock.contains("/top/@/var/lib/portables"));
    }

    #[test]
    fn recover_skips_destination_collision() {
        // The user installed something between the crashed restore and
        // the recovery run: the live @ now has its OWN subvol at
        // var/lib/portables.  We must not overwrite that — the orphan
        // gets left in the tombstone for manual review.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        // Live @ already has a subvol at the colliding path.
        mock.seed_subvolume("/top/@/var/lib/portables");

        let tombstone = toplevel.join("@-DELETE-20260101-120000");
        mock.seed_subvolume(tombstone.clone());
        mock.seed_subvolume(tombstone.join("var/lib/portables"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        assert_eq!(recovered, 0);
        // Both copies still present — the live one untouched, the
        // orphan still in the tombstone.
        assert!(mock.contains("/top/@/var/lib/portables"));
        assert!(mock.contains(tombstone.join("var/lib/portables")));
    }

    #[test]
    fn recover_handles_multiple_tombstones_and_nested() {
        // Two tombstones, each with multiple nested subvols.  All
        // should land in the live @ at their original relative paths.
        let config = config_retain_last(&["@"], 5);
        let toplevel = Path::new("/top");
        let mock = setup_mock(&config, toplevel);

        let t1 = toplevel.join("@-DELETE-20260101-120000");
        mock.seed_subvolume(t1.clone());
        mock.seed_subvolume(t1.join("var/lib/portables"));
        mock.seed_subvolume(t1.join("var/lib/machines"));

        let t2 = toplevel.join("@-DELETE-20260102-130000");
        mock.seed_subvolume(t2.clone());
        mock.seed_subvolume(t2.join("var/cache/build"));

        let recovered = recover_orphaned_nested_subvols(&config, &mock, toplevel).unwrap();
        // 3 nested rescued in total.  Note that having two tombstones
        // each carrying overlapping paths would surface as a
        // destination collision on the second one — that's tested
        // separately.
        assert_eq!(recovered, 3);
        assert!(mock.contains("/top/@/var/lib/portables"));
        assert!(mock.contains("/top/@/var/lib/machines"));
        assert!(mock.contains("/top/@/var/cache/build"));
    }

    #[test]
    fn recover_ignores_tombstones_for_unknown_bases() {
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
