//! Rendering of command results.
//!
//! Each public `print_*` function takes an [`OutputMode`] and a data
//! payload, and writes either a human-readable text block or a JSON
//! document to stdout.  JSON output shapes are defined by the private
//! `*Output` structs below so the schema is explicit and reviewable in
//! one place.  The schema is not stable during the alpha phase — see
//! the README.

use std::collections::BTreeMap;

use serde::Serialize;

use revenant_core::check::{Finding, Severity};
use revenant_core::cleanup::{CleanupSummary, PlanAction, RetentionPlan};
use revenant_core::config::RetainConfig;
use revenant_core::metadata::{SnapshotMetadata, TriggerKind};
use revenant_core::snapshot::SnapshotId;
use revenant_core::{Config, SnapshotInfo};

use crate::cli::OutputMode;

/// Serialize a value as pretty JSON.  Returns the fallback error
/// document if serialization fails — serialization of our own structs
/// should never fail in practice, but we still want *some* parseable
/// JSON on stdout rather than a partial write.
fn to_json_string<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|e| format!(r#"{{"error":"internal JSON serialization failed: {e}"}}"#))
}

/// Pretty-print a serializable value to stdout as JSON.  Appends a
/// trailing newline so the output plays well with line-oriented
/// terminals and pipes into `jq`.
fn emit_json<T: Serialize>(value: &T) {
    println!("{}", to_json_string(value));
}

/// Emit a JSON error document to stdout.  Used by `main` when running
/// under `--json` and a command returns an `Err`.
pub fn emit_json_error(msg: &str) {
    #[derive(Serialize)]
    struct ErrorOutput<'a> {
        error: &'a str,
    }
    emit_json(&ErrorOutput { error: msg });
}

// ---------------------------------------------------------------------
// metadata rendering helpers (text mode only; JSON serializes the raw
// SnapshotMetadata struct directly via serde).
// ---------------------------------------------------------------------

/// Truncate a string to at most `max` characters, appending `…` when cut.
///
/// Single pass: greedily take `max` chars; if the iterator is drained
/// the string fit, otherwise drop the last one and append an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_none() {
        head
    } else {
        // Re-take up to `max - 1` to leave room for the ellipsis.
        let mut out: String = head.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

/// Render a compact one-line summary of the metadata for humans. The
/// caller decides where to put it (list rows use a subordinate line, the
/// create/restore flows can print it underneath the primary line).
fn metadata_summary(meta: &SnapshotMetadata) -> String {
    let kind = match meta.trigger.kind {
        TriggerKind::Manual => "manual",
        TriggerKind::Pacman => "pacman",
        TriggerKind::SystemdBoot => "systemd-boot",
        TriggerKind::SystemdPeriodic => "systemd-periodic",
        TriggerKind::Unknown => "unknown",
    };

    let detail = match meta.trigger.kind {
        TriggerKind::Pacman => {
            let t = meta
                .trigger
                .pacman
                .as_ref()
                .map(|p| &p.targets[..])
                .unwrap_or(&[]);
            if t.is_empty() {
                String::new()
            } else if t.len() <= 3 {
                format!(": {}", t.join(", "))
            } else {
                format!(": {}, {}, +{}", t[0], t[1], t.len() - 2)
            }
        }
        TriggerKind::SystemdBoot | TriggerKind::SystemdPeriodic => meta
            .trigger
            .systemd
            .as_ref()
            .and_then(|s| s.unit.as_deref())
            .map_or_else(String::new, |u| format!(" ({u})")),
        _ => String::new(),
    };

    let message = meta
        .message
        .as_deref()
        .map(|m| format!(" — \"{}\"", truncate(m, 60)))
        .unwrap_or_default();

    format!("{kind}{detail}{message}")
}

// ---------------------------------------------------------------------
// list
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct SnapshotListOutput<'a> {
    snapshots: &'a [SnapshotInfo],
}

/// Render a list of snapshot records.
pub fn print_snapshot_list(mode: OutputMode, snapshots: &[SnapshotInfo]) {
    if mode.is_json() {
        emit_json(&SnapshotListOutput { snapshots });
        return;
    }

    if snapshots.is_empty() {
        println!("No snapshots found.");
        return;
    }

    println!("{:<17} {:<12} Description", "ID", "Strain");
    println!("{}", "-".repeat(60));

    for snap in snapshots {
        let description = snap
            .metadata
            .as_ref()
            .map_or_else(|| "—".to_string(), metadata_summary);
        println!("{:<17} {:<12} {}", snap.id, snap.strain, description);
    }
}

// ---------------------------------------------------------------------
// status
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct StatusOutput<'a> {
    config: &'a Config,
    /// Snapshot count per configured strain.  Uses `BTreeMap` for a
    /// stable ordering in the JSON output.
    strain_snapshots: BTreeMap<&'a str, usize>,
    snapshots_total: usize,
}

/// Render a retention config as a one-line text summary like
/// `last=10 hourly=48 daily=7`, skipping zero-valued fields so the
/// line is scannable.  Text mode only.
fn retain_summary_text(r: &RetainConfig) -> String {
    let parts = [
        ("last", r.last),
        ("hourly", r.hourly),
        ("daily", r.daily),
        ("weekly", r.weekly),
        ("monthly", r.monthly),
        ("yearly", r.yearly),
    ];
    let active: Vec<String> = parts
        .iter()
        .filter(|(_, v)| *v > 0)
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if active.is_empty() {
        "(none)".to_string()
    } else {
        active.join(" ")
    }
}

/// Print system status information.
pub fn print_status(mode: OutputMode, config: &Config, snapshots: &[SnapshotInfo]) {
    if mode.is_json() {
        let mut strain_snapshots: BTreeMap<&str, usize> = config
            .strain
            .keys()
            .map(|name| (name.as_str(), 0usize))
            .collect();
        for snap in snapshots {
            if let Some(count) = strain_snapshots.get_mut(snap.strain.as_str()) {
                *count += 1;
            }
        }
        emit_json(&StatusOutput {
            config,
            strain_snapshots,
            snapshots_total: snapshots.len(),
        });
        return;
    }

    println!("Revenant Status");
    println!("{}", "=".repeat(40));
    println!("Rootfs backend:     {}", config.sys.rootfs.backend);
    println!("Device UUID:        {}", config.sys.rootfs.device_uuid);
    println!("Rootfs subvolume:   {}", config.sys.rootfs_subvol);
    println!("Snapshot subvolume: {}", config.sys.snapshot_subvol);
    println!(
        "EFI sync:           {}",
        if config.sys.efi.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    if config.sys.efi.enabled {
        println!(
            "EFI mount point:    {}",
            config.sys.efi.mount_point.display()
        );
        println!("EFI staging subvol: {}", config.sys.efi.staging_subvol);
    }
    println!("Bootloader:         {}", config.sys.bootloader.backend);
    println!();
    println!("Strains:");
    for (name, strain) in &config.strain {
        let count = snapshots.iter().filter(|s| s.strain == *name).count();
        let r = &strain.retain;
        let retain_str = format!(
            "last={} hourly={} daily={} weekly={} monthly={} yearly={}",
            r.last, r.hourly, r.daily, r.weekly, r.monthly, r.yearly
        );
        println!(
            "  {name}: retain=[{retain_str}], subvolumes={:?}, efi={}, snapshots={count}",
            strain.subvolumes, strain.efi
        );
    }
    println!();
    println!("Total snapshots:    {}", snapshots.len());
}

// ---------------------------------------------------------------------
// cleanup --dry-run
// ---------------------------------------------------------------------

/// Render a dry-run retention plan.  JSON emits the whole [`RetentionPlan`]
/// structure verbatim (it already derives `Serialize`); text mode
/// produces the per-strain keep/delete block from before.
pub fn print_retention_plan(mode: OutputMode, plan: &RetentionPlan) {
    if mode.is_json() {
        emit_json(plan);
        return;
    }

    println!("Dry-run — no changes will be made.");
    println!();

    let mut total_kept = 0usize;
    let mut total_delete = 0usize;

    for strain in &plan.strains {
        println!(
            "Strain: {}  (retain: {})",
            strain.strain,
            retain_summary_text(&strain.retain)
        );

        if strain.entries.is_empty() {
            println!("  (no snapshots)");
        } else {
            for entry in &strain.entries {
                match &entry.action {
                    PlanAction::Keep { reasons } => {
                        let reason_str = reasons
                            .iter()
                            .map(revenant_core::retention::KeepReason::as_str)
                            .collect::<Vec<_>>()
                            .join(", ");
                        println!("  KEEP    {}   {reason_str}", entry.id);
                        total_kept += 1;
                    }
                    PlanAction::Delete => {
                        println!("  DELETE  {}   no retention rule matches", entry.id);
                        total_delete += 1;
                    }
                }
            }
        }
        println!();
    }

    if !plan.delete_markers.is_empty() {
        println!("DELETE markers (pending purge):");
        for marker in &plan.delete_markers {
            println!("  {}", marker.name);
        }
        println!();
    }

    let marker_count = plan.delete_markers.len();
    if marker_count == 0 {
        println!("Summary: {total_kept} kept, {total_delete} to delete");
    } else {
        let plural = if marker_count == 1 { "" } else { "s" };
        println!(
            "Summary: {total_kept} kept, {total_delete} to delete, {marker_count} delete marker{plural}"
        );
    }
}

// ---------------------------------------------------------------------
// cleanup (live)
// ---------------------------------------------------------------------

/// Render the result of a live cleanup run.
pub fn print_cleanup_result(mode: OutputMode, summary: &CleanupSummary) {
    if mode.is_json() {
        emit_json(summary);
        return;
    }

    if summary.removed.is_empty() && summary.removed_sidecars.is_empty() {
        println!("No snapshots to clean up.");
        return;
    }

    for id in &summary.removed {
        println!("Removed: {id}");
    }
    for name in &summary.removed_sidecars {
        println!("Removed orphaned sidecar: {name}");
    }

    if !summary.removed.is_empty() {
        println!("Cleaned up {} snapshot(s).", summary.removed.len());
    }
    if !summary.removed_sidecars.is_empty() {
        println!(
            "Removed {} orphaned sidecar(s).",
            summary.removed_sidecars.len()
        );
    }
}

// ---------------------------------------------------------------------
// snapshot
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct SnapshotCreateOutput<'a> {
    created: &'a SnapshotInfo,
    retention_removed: &'a [String],
}

/// Render the result of a `snapshot` invocation.
pub fn print_snapshot_created(mode: OutputMode, info: &SnapshotInfo, retention_removed: &[String]) {
    if mode.is_json() {
        emit_json(&SnapshotCreateOutput {
            created: info,
            retention_removed,
        });
        return;
    }

    println!("Snapshot created: {} (strain: {})", info.id, info.strain);
    if let Some(meta) = info.metadata.as_ref() {
        println!("  {}", metadata_summary(meta));
    }
    for id in retention_removed {
        println!("Retention: removed old snapshot {id}");
    }
}

// ---------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct DeleteOutput<'a> {
    strain: &'a str,
    deleted: &'a [String],
}

/// Render the result of a `delete` invocation.  `deleted` is always a
/// list even for a single snapshot delete, so consumers have one shape
/// to handle.
pub fn print_delete_result(mode: OutputMode, strain: &str, deleted: &[String], single: bool) {
    if mode.is_json() {
        emit_json(&DeleteOutput { strain, deleted });
        return;
    }

    if single {
        // Single-snapshot delete: one id expected.
        if let Some(id) = deleted.first() {
            println!("Snapshot {id} deleted.");
        }
        return;
    }

    // --all path.
    if deleted.is_empty() {
        println!("No snapshots found for strain '{strain}'.");
    } else {
        for id in deleted {
            println!("Deleted: {id}");
        }
        println!(
            "Deleted {} snapshot(s) from strain '{strain}'.",
            deleted.len()
        );
    }
}

// ---------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct RestoreRef<'a> {
    id: &'a SnapshotId,
    strain: &'a str,
}

#[derive(Serialize)]
struct RestoreRefusalOutput<'a> {
    would_restore: RestoreRef<'a>,
    subvolumes: &'a [String],
    efi_sync: bool,
    proceed_with: &'static str,
}

#[derive(Serialize)]
struct RestoreSuccessOutput<'a> {
    restored: RestoreRef<'a>,
    /// The snapshot taken of the pre-restore state when the caller
    /// passed `--save-current`.  Omitted when the flag was not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pre_restore_snapshot: Option<RestoreRef<'a>>,
    reboot_required: bool,
}

/// Render the refusal block for `restore` without `--yes`.
///
/// In text mode this writes the explanatory block to **stderr** so a
/// script capturing stdout sees nothing; in JSON mode the refusal is
/// itself the machine-readable payload and goes to **stdout** alongside
/// every other JSON result.  Both modes still exit 1 from the caller.
pub fn print_restore_refusal(
    mode: OutputMode,
    snap: &SnapshotInfo,
    subvolumes: &[String],
    efi_sync: bool,
    efi_mount_point: &std::path::Path,
) {
    if mode.is_json() {
        emit_json(&RestoreRefusalOutput {
            would_restore: RestoreRef {
                id: &snap.id,
                strain: &snap.strain,
            },
            subvolumes,
            efi_sync,
            proceed_with: "--yes",
        });
        return;
    }

    eprintln!("Restore is destructive and requires explicit confirmation.");
    eprintln!();
    eprintln!("Target snapshot: {} (strain: {})", snap.id, snap.strain);
    eprintln!("The following live subvolumes would be replaced:");
    for subvol in subvolumes {
        eprintln!(
            "  {subvol}  →  renamed to {subvol}-DELETE-<ts>, then re-created from the snapshot"
        );
    }
    if efi_sync {
        eprintln!(
            "The EFI staging area would be synced back to {}.",
            efi_mount_point.display()
        );
    }
    eprintln!();
    eprintln!(
        "The renamed {}-DELETE-<ts> subvolume(s) survive until the next `revenantctl cleanup`",
        subvolumes.first().map_or("@", String::as_str)
    );
    eprintln!("(or the next restore), so they can serve as a volatile undo buffer for the");
    eprintln!("previous live state.  No automatic pre-restore snapshot is created any more —");
    eprintln!("pass --save-current to snapshot the current state into the target strain");
    eprintln!("as part of this restore, or run `revenantctl snapshot` beforehand.");
    eprintln!();
    eprintln!("Re-run with --yes to proceed.");
}

/// Render the completion message after a successful restore.
///
/// `pre_restore` is `Some` when `--save-current` created a snapshot of
/// the previous state; callers pass `None` otherwise.  Both modes
/// surface the pre-restore snapshot so the user can see where to
/// return to if they change their mind.
pub fn print_restore_success(
    mode: OutputMode,
    snap: &SnapshotInfo,
    pre_restore: Option<&SnapshotInfo>,
) {
    if mode.is_json() {
        emit_json(&RestoreSuccessOutput {
            restored: RestoreRef {
                id: &snap.id,
                strain: &snap.strain,
            },
            pre_restore_snapshot: pre_restore.map(|p| RestoreRef {
                id: &p.id,
                strain: &p.strain,
            }),
            reboot_required: true,
        });
        return;
    }

    if let Some(p) = pre_restore {
        println!("Pre-restore snapshot: {} (strain: {})", p.id, p.strain);
    }
    println!("Restore complete. Please reboot to apply changes.");
}

// ---------------------------------------------------------------------
// check
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct CheckSummary {
    errors: usize,
    warnings: usize,
    infos: usize,
}

#[derive(Serialize)]
struct CheckOutput<'a> {
    findings: &'a [Finding],
    summary: CheckSummary,
}

fn summarize_findings(findings: &[Finding]) -> CheckSummary {
    CheckSummary {
        errors: findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count(),
        warnings: findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count(),
        infos: findings
            .iter()
            .filter(|f| f.severity == Severity::Info)
            .count(),
    }
}

/// Render a list of check findings with a summary footer.
pub fn print_findings(mode: OutputMode, findings: &[Finding]) {
    if mode.is_json() {
        emit_json(&CheckOutput {
            findings,
            summary: summarize_findings(findings),
        });
        return;
    }

    if findings.is_empty() {
        println!("All checks passed.");
        return;
    }

    for finding in findings {
        println!(
            "[{}] {}: {}",
            finding.severity.label(),
            finding.check,
            finding.message
        );
        if let Some(hint) = &finding.hint {
            println!("       hint: {hint}");
        }
    }

    let s = summarize_findings(findings);
    println!();
    println!(
        "Summary: {} error(s), {} warning(s), {} info(s)",
        s.errors, s.warnings, s.infos
    );
}

// ---------------------------------------------------------------------
// init
// ---------------------------------------------------------------------

/// One step performed by `revenantctl init`.
///
/// In JSON mode these events are collected into a list and emitted as a
/// single `{"tasks": [...]}` document at the end of the command.  In
/// text mode they are printed live as they happen, matching the existing
/// human-readable output.
#[derive(Debug, Serialize)]
#[serde(tag = "task", rename_all = "kebab-case")]
pub enum InitTask {
    /// System auto-detection results (fresh init path).
    DetectedSystem {
        backend: String,
        device_uuid: String,
        rootfs_subvol: String,
        snapshot_subvol: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        efi_mount: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        bootloader: Option<String>,
    },
    /// Config file was written.
    WroteConfig {
        path: String,
        /// `true` when the file did not exist before, `false` when it
        /// was overwritten (`--force`) or had strains added to it.
        created: bool,
    },
    /// Systemd strains inserted into an existing config that was
    /// missing them.
    AddedSystemdStrains { strains: Vec<String> },
    /// A systemd unit file was processed.  `status` is one of
    /// `written`, `unchanged`, `overwritten`.
    WroteSystemdUnit { name: String, status: String },
    /// A package-manager strain was inserted into an existing config
    /// that was missing it.  `pm` is the
    /// [`revenant_core::pkgmgr::PackageManager::name`] value, e.g.
    /// `"pacman"`, so JSON consumers can branch on it without string-
    /// matching the strain name.
    AddedPkgmgrStrain { pm: String, strain: String },
    /// A package-manager hook file was processed.  `status` is one of
    /// `written`, `unchanged`, `overwritten`.
    WrotePkgmgrHook {
        pm: String,
        name: String,
        status: String,
    },
}

/// Accumulator for `cmd_init` that knows whether it's running in text
/// or JSON mode.  In text mode it writes to stdout immediately; in JSON
/// mode it buffers [`InitTask`] entries until [`Self::finish`] is
/// called at the end of a successful `init` run.
pub struct InitReporter {
    mode: OutputMode,
    tasks: Vec<InitTask>,
}

impl InitReporter {
    #[must_use]
    pub fn new(mode: OutputMode) -> Self {
        Self {
            mode,
            tasks: Vec::new(),
        }
    }

    /// Record a task.  Prints a text line in text mode, or pushes onto
    /// the buffered list in JSON mode.
    pub fn task(&mut self, task: InitTask) {
        if self.mode.is_json() {
            self.tasks.push(task);
            return;
        }
        match &task {
            InitTask::DetectedSystem {
                backend,
                device_uuid,
                rootfs_subvol,
                snapshot_subvol,
                efi_mount,
                bootloader,
            } => {
                println!("Detecting system configuration...");
                println!("  Filesystem: {backend}");
                println!("  Device UUID: {device_uuid}");
                println!("  Rootfs subvol: {rootfs_subvol}");
                println!("  Snapshot subvol: {snapshot_subvol}");
                match efi_mount {
                    Some(p) => println!("  EFI mount: {p}"),
                    None => println!("  EFI: not detected"),
                }
                match bootloader {
                    Some(b) => println!("  Bootloader: {b}"),
                    None => println!("  Bootloader: not detected"),
                }
            }
            InitTask::WroteConfig { path, .. } => {
                println!("\nConfiguration written to {path}");
                println!(
                    "Review and adjust as needed, then create snapshots with: revenantctl snapshot"
                );
            }
            InitTask::AddedSystemdStrains { strains } => {
                for s in strains {
                    println!("Added missing strain '{s}'");
                }
            }
            InitTask::WroteSystemdUnit { name, status } => match status.as_str() {
                "unchanged" => println!("  {name} (unchanged)"),
                _ => println!("  {name}"),
            },
            InitTask::AddedPkgmgrStrain { strain, .. } => {
                println!("Added missing strain '{strain}'");
            }
            InitTask::WrotePkgmgrHook { name, status, .. } => match status.as_str() {
                "unchanged" => println!("  {name} (unchanged)"),
                _ => println!("  {name}"),
            },
        }
    }

    /// Prefix printed before the first unit file in text mode — the
    /// human output has a "Writing systemd units to <dir>/" header that
    /// does not belong in the task list.
    pub fn systemd_header(&self, dir: &str) {
        if !self.mode.is_json() {
            println!("\nWriting systemd units to {dir}/");
        }
    }

    /// Footer printed after all unit files in text mode.
    pub fn systemd_footer(&self) {
        if !self.mode.is_json() {
            println!("\nEnable boot snapshots:");
            println!("  systemctl enable revenant-boot.service");
            println!("\nEnable periodic snapshots:");
            println!("  systemctl enable --now revenant-periodic.timer");
        }
    }

    /// Prefix printed before the first hook file for a package manager
    /// in text mode.  Generic over the PM name so future backends
    /// (apt, zypp, ...) can reuse the same reporter without touching
    /// this module.
    pub fn pkgmgr_header(&self, pm: &str, dir: &str) {
        if !self.mode.is_json() {
            println!("\nWriting {pm} hook to {dir}/");
        }
    }

    /// Footer printed after all hook files for a package manager in
    /// text mode.
    pub fn pkgmgr_footer(&self, pm: &str) {
        if !self.mode.is_json() {
            println!("\nThe {pm} hook is active immediately — the next transaction will");
            println!("take a snapshot into the configured strain.");
        }
    }

    /// Flush the buffered task list as JSON.  No-op in text mode.
    pub fn finish(self) {
        if self.mode.is_json() {
            #[derive(Serialize)]
            struct InitOutput {
                tasks: Vec<InitTask>,
            }
            emit_json(&InitOutput { tasks: self.tasks });
        }
    }
}

#[cfg(test)]
mod tests {
    //! Golden tests for the JSON output shapes.
    //!
    //! The goal of these tests is to catch accidental schema changes: if
    //! a field is renamed, removed, or flattened, the corresponding
    //! `serde_json::json!` fixture here stops matching and CI fails.
    //! They deliberately do not try to exercise the text rendering
    //! paths — those are purely cosmetic and regress more noisily.

    use super::*;
    use revenant_core::check::Finding;
    use revenant_core::snapshot::{SnapshotId, SnapshotInfo};
    use serde_json::json;

    fn sample_snapshot(id: &str, strain: &str, efi: bool) -> SnapshotInfo {
        SnapshotInfo {
            id: SnapshotId::from_string(id).unwrap(),
            strain: strain.to_string(),
            subvolumes: vec!["@".to_string()],
            efi_synced: efi,
            metadata: None,
        }
    }

    #[test]
    fn list_json_shape() {
        let snaps = vec![sample_snapshot("20260411-080000", "default", true)];
        let out = SnapshotListOutput { snapshots: &snaps };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&out)).unwrap();
        assert_eq!(
            got,
            json!({
                "snapshots": [{
                    "id": "20260411-080000",
                    "strain": "default",
                    "subvolumes": ["@"],
                    "efi_synced": true,
                }]
            })
        );
    }

    #[test]
    fn list_json_empty() {
        let out = SnapshotListOutput { snapshots: &[] };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&out)).unwrap();
        assert_eq!(got, json!({ "snapshots": [] }));
    }

    #[test]
    fn list_json_includes_metadata_when_present() {
        use revenant_core::metadata::{SnapshotMetadata, Trigger};
        let mut snap = sample_snapshot("20260411-080000", "pacman", false);
        snap.metadata = Some(SnapshotMetadata::new(
            Some("test".into()),
            Trigger::pacman(vec!["linux".into(), "mesa".into()]),
        ));
        let out = SnapshotListOutput {
            snapshots: std::slice::from_ref(&snap),
        };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&out)).unwrap();
        let meta = &got["snapshots"][0]["metadata"];
        assert_eq!(meta["message"], json!("test"));
        assert_eq!(meta["trigger"]["kind"], json!("pacman"));
        assert_eq!(
            meta["trigger"]["pacman"]["targets"],
            json!(["linux", "mesa"])
        );
    }

    #[test]
    fn list_json_omits_metadata_when_absent() {
        let snap = sample_snapshot("20260411-080000", "default", false);
        let out = SnapshotListOutput {
            snapshots: std::slice::from_ref(&snap),
        };
        let s = to_json_string(&out);
        assert!(
            !s.contains("\"metadata\""),
            "metadata field should be omitted when None"
        );
    }

    #[test]
    fn metadata_summary_manual_with_message() {
        use revenant_core::metadata::{SnapshotMetadata, Trigger};
        let meta = SnapshotMetadata::new(Some("pre-upgrade".into()), Trigger::manual());
        let s = metadata_summary(&meta);
        assert!(s.starts_with("manual"));
        assert!(s.contains("pre-upgrade"));
    }

    #[test]
    fn metadata_summary_pacman_truncates_long_target_list() {
        use revenant_core::metadata::{SnapshotMetadata, Trigger};
        let meta = SnapshotMetadata::new(
            None,
            Trigger::pacman(vec![
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
                "e".into(),
            ]),
        );
        let s = metadata_summary(&meta);
        assert!(s.contains("pacman"));
        assert!(s.contains("a, b, +3"));
    }

    #[test]
    fn metadata_summary_systemd_shows_unit() {
        use revenant_core::metadata::{SnapshotMetadata, Trigger, TriggerKind};
        let meta = SnapshotMetadata::new(
            None,
            Trigger::systemd(
                TriggerKind::SystemdBoot,
                Some("revenant-boot.service".into()),
            ),
        );
        let s = metadata_summary(&meta);
        assert!(s.contains("systemd-boot"));
        assert!(s.contains("revenant-boot.service"));
    }

    #[test]
    fn truncate_longer_than_max() {
        assert_eq!(truncate("hello", 3), "he…");
        assert_eq!(truncate("hi", 3), "hi");
    }

    #[test]
    fn check_json_summary_counts() {
        let findings = vec![
            Finding::error("config-missing", "nope"),
            Finding::warning("orphaned-snapshot", "stale"),
            Finding::info("nested-subvolumes", "just fyi"),
            Finding::warning("orphaned-snapshot", "another"),
        ];
        let summary = summarize_findings(&findings);
        let out = CheckOutput {
            findings: &findings,
            summary,
        };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&out)).unwrap();
        assert_eq!(got["summary"]["errors"], json!(1));
        assert_eq!(got["summary"]["warnings"], json!(2));
        assert_eq!(got["summary"]["infos"], json!(1));
        // Severity is serialized in lowercase.
        assert_eq!(got["findings"][0]["severity"], json!("error"));
        assert_eq!(got["findings"][1]["severity"], json!("warning"));
        assert_eq!(got["findings"][2]["severity"], json!("info"));
    }

    #[test]
    fn check_json_optional_hint_omitted() {
        let findings = vec![Finding::warning("x", "no hint here")];
        let summary = summarize_findings(&findings);
        let out = CheckOutput {
            findings: &findings,
            summary,
        };
        let s = to_json_string(&out);
        // skip_serializing_if kept `hint` out of the object entirely.
        assert!(
            !s.contains("\"hint\""),
            "hint should be omitted when None, got: {s}"
        );
    }

    #[test]
    fn snapshot_create_json_shape() {
        let snap = sample_snapshot("20260411-090000", "default", false);
        let removed = vec!["20260410-010000".to_string()];
        let out = SnapshotCreateOutput {
            created: &snap,
            retention_removed: &removed,
        };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&out)).unwrap();
        assert_eq!(got["created"]["id"], json!("20260411-090000"));
        assert_eq!(got["created"]["strain"], json!("default"));
        assert_eq!(got["retention_removed"], json!(["20260410-010000"]));
    }

    #[test]
    fn delete_json_shape() {
        let deleted = vec!["20260411-080000".to_string()];
        let out = DeleteOutput {
            strain: "default",
            deleted: &deleted,
        };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&out)).unwrap();
        assert_eq!(
            got,
            json!({
                "strain": "default",
                "deleted": ["20260411-080000"],
            })
        );
    }

    #[test]
    fn restore_refusal_json_shape() {
        let snap = sample_snapshot("20260411-080000", "default", true);
        let subvolumes = vec!["@".to_string()];
        let out = RestoreRefusalOutput {
            would_restore: RestoreRef {
                id: &snap.id,
                strain: &snap.strain,
            },
            subvolumes: &subvolumes,
            efi_sync: true,
            proceed_with: "--yes",
        };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&out)).unwrap();
        assert_eq!(
            got,
            json!({
                "would_restore": {"id": "20260411-080000", "strain": "default"},
                "subvolumes": ["@"],
                "efi_sync": true,
                "proceed_with": "--yes",
            })
        );
    }

    #[test]
    fn restore_success_json_shape() {
        let snap = sample_snapshot("20260411-080000", "default", false);
        let out = RestoreSuccessOutput {
            restored: RestoreRef {
                id: &snap.id,
                strain: &snap.strain,
            },
            pre_restore_snapshot: None,
            reboot_required: true,
        };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&out)).unwrap();
        assert_eq!(
            got,
            json!({
                "restored": {"id": "20260411-080000", "strain": "default"},
                "reboot_required": true,
            })
        );
    }

    #[test]
    fn restore_success_json_includes_pre_snapshot() {
        let snap = sample_snapshot("20260411-080000", "default", false);
        let pre = sample_snapshot("20260411-075500", "default", false);
        let out = RestoreSuccessOutput {
            restored: RestoreRef {
                id: &snap.id,
                strain: &snap.strain,
            },
            pre_restore_snapshot: Some(RestoreRef {
                id: &pre.id,
                strain: &pre.strain,
            }),
            reboot_required: true,
        };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&out)).unwrap();
        assert_eq!(
            got,
            json!({
                "restored": {"id": "20260411-080000", "strain": "default"},
                "pre_restore_snapshot": {"id": "20260411-075500", "strain": "default"},
                "reboot_required": true,
            })
        );
    }

    #[test]
    fn restore_success_json_omits_pre_snapshot_when_none() {
        let snap = sample_snapshot("20260411-080000", "default", false);
        let out = RestoreSuccessOutput {
            restored: RestoreRef {
                id: &snap.id,
                strain: &snap.strain,
            },
            pre_restore_snapshot: None,
            reboot_required: true,
        };
        let s = to_json_string(&out);
        assert!(
            !s.contains("\"pre_restore_snapshot\""),
            "pre_restore_snapshot should be omitted when None, got: {s}"
        );
    }

    #[test]
    fn cleanup_result_json_shape() {
        let summary = CleanupSummary {
            removed: vec!["20260410-010000".to_string(), "20260410-020000".to_string()],
            removed_sidecars: vec![],
        };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&summary)).unwrap();
        assert_eq!(
            got,
            json!({
                "removed": ["20260410-010000", "20260410-020000"],
                "removed_sidecars": [],
            })
        );
    }

    #[test]
    fn cleanup_result_json_includes_sidecars() {
        let summary = CleanupSummary {
            removed: vec![],
            removed_sidecars: vec!["@-default-20260410-010000.meta.toml".to_string()],
        };
        let got: serde_json::Value = serde_json::from_str(&to_json_string(&summary)).unwrap();
        assert_eq!(
            got,
            json!({
                "removed": [],
                "removed_sidecars": ["@-default-20260410-010000.meta.toml"],
            })
        );
    }

    #[test]
    fn init_task_tag_kebab_case() {
        // Schema check: the `task` discriminator is emitted in kebab-case
        // so consumers see `"wrote-config"`, not `"WroteConfig"`.
        let task = InitTask::WroteConfig {
            path: "/etc/revenant/config.toml".to_string(),
            created: true,
        };
        let s = to_json_string(&task);
        assert!(s.contains("\"task\": \"wrote-config\""), "got: {s}");
        assert!(s.contains("\"created\": true"), "got: {s}");

        let task = InitTask::WroteSystemdUnit {
            name: "revenant-boot.service".to_string(),
            status: "written".to_string(),
        };
        let s = to_json_string(&task);
        assert!(s.contains("\"task\": \"wrote-systemd-unit\""), "got: {s}");
    }

    #[test]
    fn init_task_added_pkgmgr_strain_tag_kebab_case() {
        // Schema lock: the discriminator is `added-pkgmgr-strain` and
        // the `pm` field carries the PackageManager::name() value.
        let task = InitTask::AddedPkgmgrStrain {
            pm: "pacman".to_string(),
            strain: "pacman".to_string(),
        };
        let s = to_json_string(&task);
        let got: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(got["task"], json!("added-pkgmgr-strain"));
        assert_eq!(got["pm"], json!("pacman"));
        assert_eq!(got["strain"], json!("pacman"));
    }

    #[test]
    fn init_task_wrote_pkgmgr_hook_tag_kebab_case() {
        let task = InitTask::WrotePkgmgrHook {
            pm: "pacman".to_string(),
            name: "50-revenant-snapshot.hook".to_string(),
            status: "written".to_string(),
        };
        let s = to_json_string(&task);
        let got: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(got["task"], json!("wrote-pkgmgr-hook"));
        assert_eq!(got["pm"], json!("pacman"));
        assert_eq!(got["name"], json!("50-revenant-snapshot.hook"));
        assert_eq!(got["status"], json!("written"));
    }

    #[test]
    fn init_detected_system_omits_missing_efi() {
        // Optional fields use skip_serializing_if so machines without
        // EFI / known bootloaders still produce a clean document.
        let task = InitTask::DetectedSystem {
            backend: "btrfs".to_string(),
            device_uuid: "abcd-1234".to_string(),
            rootfs_subvol: "@".to_string(),
            snapshot_subvol: "@snapshots".to_string(),
            efi_mount: None,
            bootloader: None,
        };
        let s = to_json_string(&task);
        assert!(!s.contains("efi_mount"), "got: {s}");
        assert!(!s.contains("bootloader"), "got: {s}");
    }

    #[test]
    fn error_output_shape() {
        // The error envelope used by `emit_json_error`.
        #[derive(Serialize)]
        struct E<'a> {
            error: &'a str,
        }
        let s = to_json_string(&E {
            error: "boom happened",
        });
        let got: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(got, json!({"error": "boom happened"}));
    }
}
