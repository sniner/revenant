//! Package-manager hook integration.
//!
//! Each supported package manager is represented by a type implementing
//! [`PackageManager`]. The trait is intentionally thin: it produces a
//! list of [`HookFile`]s (pure strings) for a given [`HookParams`], and
//! names the default directory where those files live on disk. The CLI
//! layer handles writing the files and reporting idempotent status.
//!
//! The first concrete implementation is [`pacman::Pacman`]; apt and
//! zypp are expected to follow the same shape.

pub mod pacman;

use std::path::{Path, PathBuf};

/// Input for hook generation. Shared across every package manager
/// backend so the CLI can produce a single set of parameters and pass
/// them through a trait object.
pub struct HookParams {
    /// Absolute path to the `revenantctl` binary that the hook should invoke.
    pub bin_path: PathBuf,
    /// Absolute path to the revenant configuration file.
    pub config_path: PathBuf,
    /// Strain to snapshot into on each package-manager transaction.
    pub strain: String,
}

/// A generated hook file ready to be written to disk. The filename is
/// relative (no directory component) — the CLI combines it with the
/// user-selected hook directory.
pub struct HookFile {
    pub filename: String,
    pub content: String,
}

/// Abstraction over package-manager hook integration.
///
/// Implementations are pure string generators: no I/O happens inside
/// [`generate_hooks`]. That keeps them trivially unit-testable and
/// makes the CLI layer the single place where files are written.
pub trait PackageManager: Send + Sync {
    /// Short stable identifier used in logs, JSON output, and CLI
    /// flag names (`"pacman"`, later `"apt"`, `"zypp"`, ...).
    fn name(&self) -> &'static str;

    /// Default filesystem location for this PM's hook files.
    fn default_hook_dir(&self) -> &Path;

    /// Render the hook files that should be installed for this PM.
    fn generate_hooks(&self, params: &HookParams) -> Vec<HookFile>;

    /// Paths, relative to the rootfs subvolume, that revenant should
    /// delete from the restored tree before declaring a restore
    /// complete. Used to strip runtime state that the package manager
    /// holds open across its own hooks — most notably lock files that
    /// the PM acquires *before* the PreTransaction hook fires, and
    /// that therefore end up baked into every hook-triggered snapshot.
    ///
    /// On pacman this is `var/lib/pacman/db.lck`: pacman grabs the
    /// lock, fires our PreTransaction hook, we snapshot the live tree
    /// (including the lock), and the transaction later releases the
    /// lock in the live tree — but the *snapshot* still carries it.
    /// After a restore the live rootfs then has a stale lock that
    /// pacman refuses to run against until it is removed by hand.
    /// Cleaning these up during restore short-circuits that footgun.
    ///
    /// Paths are stripped from the restored rootfs unconditionally:
    /// a file that is not there is treated as already-clean. Default
    /// is empty, so package managers that do not need cleanup do not
    /// have to override anything.
    fn stale_runtime_files(&self) -> &[&'static str] {
        &[]
    }
}

/// All package managers revenant knows about.
///
/// This is the single source of truth for code that needs to iterate
/// over every supported PM regardless of whether the user configured
/// it — for example, the restore path's stale-runtime-file cleanup,
/// which must run even on systems that never invoked `init --pacman`
/// (the file either exists and needs removing, or it does not and
/// the cleanup is a cheap no-op).
#[must_use]
pub fn all_package_managers() -> Vec<Box<dyn PackageManager>> {
    vec![Box::new(pacman::Pacman)]
}
