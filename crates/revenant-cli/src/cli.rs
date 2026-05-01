use clap::{Parser, Subcommand, ValueEnum};

/// Revenant -- System snapshot tool for Linux
#[derive(Debug, Parser)]
#[command(name = "revenantctl", version, about)]
pub struct Cli {
    /// Path to configuration file
    #[arg(long, default_value = revenant_core::config::DEFAULT_CONFIG_PATH)]
    pub config: String,

    /// Increase verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Emit machine-readable JSON on stdout instead of human-readable
    /// text.  Errors are reported as `{"error": "..."}` on stdout with
    /// exit code 1.  The schema is not stable during the alpha phase.
    #[arg(short = 'j', long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Which rendering mode the CLI is operating in.
///
/// Derived from the global `--json` flag at startup and passed down to
/// every command so that the same `cmd_*` function can produce either
/// human-readable text or machine-readable JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Text,
    Json,
}

impl OutputMode {
    #[must_use]
    pub fn is_json(self) -> bool {
        matches!(self, OutputMode::Json)
    }
}

/// CLI-facing mirror of `revenant_core::metadata::TriggerKind`. Kept in
/// this crate so `clap`'s `ValueEnum` derive stays out of `revenant-core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum TriggerKindArg {
    Manual,
    Pacman,
    SystemdBoot,
    SystemdPeriodic,
    Unknown,
}

impl From<TriggerKindArg> for revenant_core::metadata::TriggerKind {
    fn from(v: TriggerKindArg) -> Self {
        use revenant_core::metadata::TriggerKind;
        match v {
            TriggerKindArg::Manual => TriggerKind::Manual,
            TriggerKindArg::Pacman => TriggerKind::Pacman,
            TriggerKindArg::SystemdBoot => TriggerKind::SystemdBoot,
            TriggerKindArg::SystemdPeriodic => TriggerKind::SystemdPeriodic,
            TriggerKindArg::Unknown => TriggerKind::Unknown,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new snapshot in the given strain.
    Snapshot {
        /// Strain to snapshot. Defaults to `default`.
        #[arg(default_value = "default")]
        strain: String,
        /// Free-form message item stored in the snapshot's metadata
        /// sidecar. Repeat the flag to attach multiple items (e.g. one
        /// per package). Useful for labelling manual snapshots.
        #[arg(short, long = "message")]
        message: Vec<String>,
        /// Trigger kind recorded in metadata. Defaults to `manual`.
        /// Package-manager hooks and systemd units pass this themselves;
        /// it is rarely useful on the command line directly.
        #[arg(long, value_enum, hide = true)]
        trigger: Option<TriggerKindArg>,
        /// Read additional message items from stdin, one per line. Used
        /// by the pacman hook to feed the package list (`NeedsTargets`).
        #[arg(long, hide = true)]
        from_stdin: bool,
        /// Mark the new snapshot as protected: it is excluded from
        /// retention and refuses manual deletion until cleared via
        /// `revenantctl edit <strain>@<id> --unprotect`.
        #[arg(long)]
        protect: bool,
    },
    /// List snapshots, optionally filtered by strain (e.g. `default@`).
    List {
        /// Optional filter:
        ///   `strain@`     — show only snapshots in that strain
        ///   `strain@all`  — alias for `strain@`
        ///   omitted       — show every snapshot
        ///
        /// `strain@ID` and bare `ID` forms are also accepted and shown
        /// as a single-row list, useful for confirming a snapshot's
        /// metadata.
        target: Option<String>,
    },
    /// Restore a snapshot. The argument is `strain@ID` (fully qualified) or a
    /// bare `ID` (auto-resolved across strains; ambiguous IDs error).
    Restore {
        /// `strain@ID` or bare `ID`. The bulk forms (`strain@`,
        /// `strain@all`) are rejected — restore acts on one snapshot.
        snapshot: String,
        /// Confirm the destructive restore. Without this flag, the
        /// command only prints what would happen and exits with code 1.
        #[arg(long)]
        yes: bool,
        /// Proceed even when pre-flight checks report blocking issues
        /// (e.g. a registered systemd-machined machine is currently
        /// running). Distinct from `--yes`: that confirms the restore
        /// itself, this overrides safety guards around the live system.
        #[arg(long)]
        force: bool,
        /// Snapshot the current state into the snapshot's strain
        /// before replacing it, so the user has a named copy to return
        /// to if the restore turns out to be unwanted. Retention is
        /// intentionally skipped for this safety snapshot — the source
        /// snapshot must remain available for the duration of the
        /// restore. The next regular `snapshot` reapplies retention.
        #[arg(long)]
        save_current: bool,
    },
    /// Delete a snapshot, or every snapshot of a strain.
    ///
    /// Targets:
    ///   `strain@ID`   — single snapshot, fully qualified
    ///   `ID`          — single snapshot, auto-resolved across strains
    ///   `strain@`     — every snapshot in that strain (bulk)
    ///   `strain@all`  — alias for `strain@`
    Delete {
        /// `strain@ID` / `ID` for single-snapshot delete, or
        /// `strain@` / `strain@all` for bulk delete of a strain.
        target: String,
    },
    /// Apply retention policy and remove old snapshots
    Cleanup {
        /// Show what would be done without touching anything.  Prints a
        /// per-strain keep/delete plan with the rule(s) protecting each
        /// kept snapshot, plus any DELETE markers that would be purged.
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Purge every DELETE marker now, ignoring `tombstone_max_age_days`.
        /// Per-strain retention rules are still honoured — `--force`
        /// only bypasses the tombstone undo window.  Combine with
        /// `--dry-run` to preview the forced purge.
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Edit metadata of an existing snapshot (sidecar fields only).
    ///
    /// Targets a single snapshot — bulk forms (`strain@`, `strain@all`)
    /// are rejected. The snapshot must already have a sidecar; pure
    /// subvolume snapshots without metadata cannot be edited.
    Edit {
        /// `strain@ID` or bare `ID` (auto-resolved across strains).
        target: String,
        /// Replace the message lines. Pass multiple times for multiple
        /// lines. Omitted means "leave the existing messages alone";
        /// passing once with an empty string clears them.
        #[arg(short, long = "message")]
        message: Option<Vec<String>>,
        /// Mark the snapshot as protected: excluded from retention,
        /// refuses manual deletion until cleared.
        #[arg(long, conflicts_with = "unprotect")]
        protect: bool,
        /// Clear the protected flag.
        #[arg(long)]
        unprotect: bool,
    },
    /// Show configuration and filesystem status
    Status,
    /// Run system health checks (config, orphaned snapshots, nested subvolumes)
    Check,
    /// Auto-detect system configuration and generate config file
    Init {
        /// Output path for the generated configuration file
        #[arg(short, long, default_value = "/etc/revenant/config.toml")]
        output: String,
        /// Overwrite generated systemd unit / pacman hook files when
        /// their on-disk content differs. Does NOT overwrite an
        /// existing config.toml — site-local edits are always kept.
        /// To regenerate the config from system detection, remove
        /// /etc/revenant/config.toml first and re-run `init`.
        #[arg(long)]
        force: bool,
        /// Generate systemd service and timer unit files
        #[arg(long)]
        systemd: bool,
        /// Directory for systemd unit files
        #[arg(long, default_value = "/etc/systemd/system")]
        systemd_dir: String,
        /// Path to the revenantctl binary (auto-detected if omitted)
        #[arg(long)]
        bin_path: Option<String>,
        /// Systemd timer calendar expression for periodic snapshots
        #[arg(long, default_value = "hourly")]
        timer_interval: String,
        /// Strain name for periodic timer snapshots
        #[arg(long, default_value = "periodic")]
        periodic_strain: String,
        /// Strain name for boot-time snapshots
        #[arg(long, default_value = "boot")]
        boot_strain: String,
        /// Generate a pacman hook for pre-transaction snapshots
        #[arg(long)]
        pacman: bool,
        /// Directory for pacman hook files
        #[arg(long, default_value = "/etc/pacman.d/hooks")]
        pacman_dir: String,
        /// Strain name for pacman pre-transaction snapshots
        #[arg(long, default_value = "pacman")]
        pacman_strain: String,
    },
}
