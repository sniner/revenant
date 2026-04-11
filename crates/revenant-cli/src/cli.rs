use clap::{Parser, Subcommand};

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

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new snapshot
    Snapshot {
        /// Strain to use for the snapshot
        #[arg(short, long, default_value = "default")]
        strain: String,
    },
    /// List all snapshots
    List {
        /// Filter by strain name
        #[arg(short, long)]
        strain: Option<String>,
    },
    /// Restore a snapshot
    Restore {
        /// Snapshot ID (format: YYYYMMDD-HHMMSS)
        snapshot_id: String,
        /// Strain (required if snapshot ID exists in multiple strains)
        #[arg(short, long)]
        strain: Option<String>,
        /// Confirm the destructive restore. Without this flag, the
        /// command only prints what would happen and exits with code 1.
        #[arg(long)]
        yes: bool,
    },
    /// Delete a snapshot (or all snapshots of a strain with --strain and --all)
    Delete {
        /// Snapshot ID (format: YYYYMMDD-HHMMSS)
        snapshot_id: Option<String>,
        /// Strain (required if snapshot ID exists in multiple strains, or with --all)
        #[arg(short, long)]
        strain: Option<String>,
        /// Delete all snapshots of the given strain
        #[arg(short, long, requires = "strain")]
        all: bool,
    },
    /// Apply retention policy and remove old snapshots
    Cleanup {
        /// Show what would be done without touching anything.  Prints a
        /// per-strain keep/delete plan with the rule(s) protecting each
        /// kept snapshot, plus any DELETE markers that would be purged.
        #[arg(short = 'n', long)]
        dry_run: bool,
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
        /// Overwrite existing configuration file
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
