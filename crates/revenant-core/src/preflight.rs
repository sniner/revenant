//! Operation-gating checks for revenant commands.
//!
//! [`crate::check`] answers "what is the state of this system?" — the
//! audit surface for `revenantctl check`. This module answers "is this
//! operation safe to run *right now*?" and is consulted by individual
//! commands before they take destructive action. Both layers share the
//! [`Finding`] / [`Severity`] vocabulary, but they have different call
//! sites, different aggregation rules, and intentionally different
//! lifetimes: an audit finding is informational, a pre-flight finding
//! gates an operation.

use std::path::Path;

use crate::check::Finding;

/// Stable identifier for the active-nspawn-machines pre-flight finding.
pub const CHECK_ACTIVE_NSPAWN_MACHINES: &str = "active-nspawn-machines";

/// Default path systemd-machined uses to publish registered machines.
pub const MACHINED_RUNTIME_DIR: &str = "/run/systemd/machines";

/// Aggregate every pre-flight check that gates [`crate::restore::restore_snapshot`].
///
/// Returns a list of [`Finding`]s. Callers gate on the highest severity
/// observed: any [`Severity::Error`] should block the operation unless
/// the user has explicitly opted to proceed (e.g. via a `--force` flag).
///
/// Add new checks here as they are implemented; keeping the aggregation
/// in one place means callers do not need to know which checks exist.
#[must_use]
pub fn preflight_restore(machined_runtime_dir: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.extend(find_active_nspawn_machines(machined_runtime_dir));
    findings
}

/// Detect machines registered with systemd-machined.
///
/// `systemd-machined` publishes one file per registered machine in
/// `/run/systemd/machines/` (the path is documented as part of its
/// runtime API). A non-empty directory means at least one machine is
/// active; restoring while such a machine runs renames the live `@`
/// subvolume and moves `var/lib/machines` into the freshly restored
/// tree, after which path lookups through the still-mounted old `@`
/// stop resolving. New host-side operations against
/// `/var/lib/machines/...` then fail until the user reboots.
///
/// Best-effort: a bare `systemd-nspawn -D ...` invocation that does not
/// register with machined is not detected here. The common entry points
/// (`machinectl start`, `systemd-nspawn@foo.service`) both register, so
/// this covers the realistic breakage surface without resorting to
/// process scans or D-Bus introspection.
///
/// A missing directory is treated as "machined never ran" and produces
/// no findings, since revenant has nothing to guard against in that
/// case. An I/O error on the read is logged and also returns no
/// findings: a partial check that fails closed (turning every restore
/// into an error because we could not enumerate `/run/systemd/`) would
/// be worse than a permissive one given the narrow real-world risk.
#[must_use]
pub fn find_active_nspawn_machines(runtime_dir: &Path) -> Vec<Finding> {
    let entries = match std::fs::read_dir(runtime_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(
                "could not enumerate {} for nspawn machine detection: {e}",
                runtime_dir.display(),
            );
            return Vec::new();
        }
    };

    let mut names: Vec<String> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|e| e.file_name().to_str().map(ToString::to_string))
        .collect();

    if names.is_empty() {
        return Vec::new();
    }

    names.sort();
    let listed = names.join(", ");
    let message = if names.len() == 1 {
        format!("nspawn machine registered with systemd-machined is active: {listed}")
    } else {
        format!("nspawn machines registered with systemd-machined are active: {listed}")
    };
    vec![
        Finding::error(CHECK_ACTIVE_NSPAWN_MACHINES, message).with_hint(
            "stop the machine(s) with `machinectl terminate <name>` before restoring, \
             or re-run with `--force` to proceed anyway",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::Severity;

    fn tmpdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("revenant-preflight-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn no_runtime_dir_yields_no_findings() {
        // machined never ran on this host: the directory simply does
        // not exist. Nothing to guard against.
        let missing = std::env::temp_dir().join(format!(
            "revenant-preflight-missing-{}",
            uuid::Uuid::new_v4()
        ));
        assert!(!missing.exists());
        let findings = find_active_nspawn_machines(&missing);
        assert!(findings.is_empty());
    }

    #[test]
    fn empty_runtime_dir_yields_no_findings() {
        // machined is present but has no machines registered.
        let dir = tmpdir();
        let findings = find_active_nspawn_machines(&dir);
        assert!(findings.is_empty(), "got: {findings:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn populated_runtime_dir_yields_one_error_finding() {
        let dir = tmpdir();
        std::fs::write(dir.join("arch-build"), "").unwrap();
        std::fs::write(dir.join("debian-test"), "").unwrap();

        let findings = find_active_nspawn_machines(&dir);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.check, CHECK_ACTIVE_NSPAWN_MACHINES);
        // Names are sorted so the message is deterministic.
        assert!(
            f.message.contains("arch-build, debian-test"),
            "expected sorted machine list in message, got: {}",
            f.message
        );
        assert!(f.hint.is_some(), "expected hint on the finding");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finding_carries_stable_check_id() {
        // Stability matters: downstream filtering / suppression keys off
        // this constant. Bumping it is a breaking change.
        let dir = tmpdir();
        std::fs::write(dir.join("c"), "").unwrap();
        let findings = find_active_nspawn_machines(&dir);
        assert_eq!(findings[0].check, "active-nspawn-machines");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preflight_restore_aggregates() {
        // Sanity-check: the aggregator surfaces findings produced by
        // its individual checks.
        let dir = tmpdir();
        std::fs::write(dir.join("foo"), "").unwrap();
        let findings = preflight_restore(&dir);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, CHECK_ACTIVE_NSPAWN_MACHINES);
        std::fs::remove_dir_all(&dir).ok();
    }
}
