//! Pacman hook generator.
//!
//! Writes a single PreTransaction hook that snapshots the system before
//! every package install, upgrade or removal. See the README section on
//! "Pacman integration" for the user-facing contract.

use std::path::Path;

use super::{HookFile, HookParams, PackageManager};

/// The stock Arch Linux hook directory for admin/package hooks.
const DEFAULT_HOOK_DIR: &str = "/etc/pacman.d/hooks";

/// Filename of the generated hook. The `50-` prefix places revenant in
/// the middle of the alphabetic ordering pacman applies to hooks.
const HOOK_FILENAME: &str = "50-revenant-snapshot.hook";

/// Pacman (Arch Linux and derivatives).
pub struct Pacman;

impl PackageManager for Pacman {
    fn name(&self) -> &'static str {
        "pacman"
    }

    fn default_hook_dir(&self) -> &Path {
        Path::new(DEFAULT_HOOK_DIR)
    }

    fn generate_hooks(&self, params: &HookParams) -> Vec<HookFile> {
        // Snapshot failures must not abort the pacman transaction: a
        // missing snapshot is a convenience failure, whereas blocking
        // the install is strictly more disruptive. The `|| true`
        // wrapper ensures the hook always exits 0; revenant's own
        // diagnostics still land on stderr and show up in pacman's
        // output.
        //
        // `NeedsTargets` in [Trigger] makes pacman feed the affected
        // package names to the hook on stdin; `revenantctl snapshot`
        // appends them to the metadata message when `--from-stdin` is
        // passed.
        let exec = format!(
            "/bin/sh -c '{} --config {} snapshot {} --trigger pacman --from-stdin || true'",
            params.bin_path.display(),
            params.config_path.display(),
            params.strain,
        );

        let content = format!(
            "\
[Trigger]
Operation = Install
Operation = Upgrade
Operation = Remove
Type = Package
Target = *

[Action]
Description = Revenant pre-transaction snapshot
When = PreTransaction
Exec = {exec}
Depends = coreutils
NeedsTargets
"
        );

        vec![HookFile {
            filename: HOOK_FILENAME.to_string(),
            content,
        }]
    }

    fn stale_runtime_files(&self) -> &[&'static str] {
        // Pacman acquires /var/lib/pacman/db.lck *before* it runs the
        // PreTransaction hooks, so the hook-triggered snapshot always
        // contains the lock file. Restoring that snapshot brings a
        // stale lock back to a live tree where no pacman process is
        // running, and the next pacman invocation then fails with
        // "unable to lock database". Strip it during restore.
        &["var/lib/pacman/db.lck"]
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn test_params() -> HookParams {
        HookParams {
            bin_path: PathBuf::from("/usr/local/bin/revenantctl"),
            config_path: PathBuf::from("/etc/revenant/config.toml"),
            strain: "pacman".to_string(),
        }
    }

    #[test]
    fn default_hook_dir_is_pacman_d_hooks() {
        assert_eq!(Pacman.default_hook_dir(), Path::new("/etc/pacman.d/hooks"));
    }

    #[test]
    fn generates_one_hook() {
        let hooks = Pacman.generate_hooks(&test_params());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].filename, "50-revenant-snapshot.hook");
    }

    #[test]
    fn hook_has_pretransaction_when() {
        let hooks = Pacman.generate_hooks(&test_params());
        let content = &hooks[0].content;
        assert!(content.contains("[Trigger]"));
        assert!(content.contains("[Action]"));
        assert!(content.contains("When = PreTransaction"));
        // We deliberately do NOT want PostTransaction in the same file.
        assert!(!content.contains("PostTransaction"));
    }

    #[test]
    fn hook_targets_all_packages() {
        let hooks = Pacman.generate_hooks(&test_params());
        let content = &hooks[0].content;
        assert!(content.contains("Operation = Install"));
        assert!(content.contains("Operation = Upgrade"));
        assert!(content.contains("Operation = Remove"));
        assert!(content.contains("Type = Package"));
        assert!(content.contains("Target = *"));
    }

    #[test]
    fn hook_contains_strain_name() {
        let mut params = test_params();
        params.strain = "pacman-custom".to_string();
        let hooks = Pacman.generate_hooks(&params);
        assert!(hooks[0].content.contains("snapshot pacman-custom"));
    }

    #[test]
    fn hook_requests_targets_on_stdin() {
        let hooks = Pacman.generate_hooks(&test_params());
        let content = &hooks[0].content;
        assert!(
            content.contains("NeedsTargets"),
            "hook must set NeedsTargets so stdin carries the package list"
        );
        assert!(
            content.contains("--trigger pacman"),
            "Exec must pass --trigger pacman so the snapshot is tagged correctly"
        );
        assert!(
            content.contains("--from-stdin"),
            "Exec must pass --from-stdin so revenantctl reads the package list from stdin"
        );
    }

    #[test]
    fn hook_is_non_blocking_via_or_true() {
        // Sanity-check that the `|| true` fallback is wired up so a
        // snapshot failure does not abort the transaction.
        let hooks = Pacman.generate_hooks(&test_params());
        assert!(hooks[0].content.contains("|| true"));
    }

    #[test]
    fn custom_bin_path_propagates() {
        let mut params = test_params();
        params.bin_path = PathBuf::from("/opt/revenant/revenantctl");
        let hooks = Pacman.generate_hooks(&params);
        assert!(hooks[0].content.contains("/opt/revenant/revenantctl"));
    }

    #[test]
    fn custom_config_path_propagates() {
        let mut params = test_params();
        params.config_path = PathBuf::from("/home/test/revenant.toml");
        let hooks = Pacman.generate_hooks(&params);
        assert!(
            hooks[0]
                .content
                .contains("--config /home/test/revenant.toml")
        );
    }

    #[test]
    fn pm_name() {
        assert_eq!(Pacman.name(), "pacman");
    }

    #[test]
    fn stale_runtime_files_lists_db_lck() {
        // Must be a relative path (no leading slash) so the restore
        // path can join it onto the rootfs subvolume root.
        let stale = Pacman.stale_runtime_files();
        assert_eq!(stale, &["var/lib/pacman/db.lck"]);
        assert!(
            !stale[0].starts_with('/'),
            "stale_runtime_files must return rootfs-relative paths"
        );
    }
}
