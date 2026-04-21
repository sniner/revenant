mod cli;
mod output;

use std::path::Path;
use std::process;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use revenant_core::backend::btrfs::BtrfsBackend;
use revenant_core::check::{self, Finding, Severity};
use revenant_core::preflight;
use revenant_core::snapshot::{self, SnapshotId};
use revenant_core::{Config, FileSystemBackend};

use revenant_core::pkgmgr::{self, PackageManager};

use crate::cli::OutputMode;
use crate::output::{InitReporter, InitTask};

fn main() {
    // Restore the default SIGPIPE handler. Rust ignores SIGPIPE by
    // default, which turns writes to a closed pipe into `EPIPE` errors
    // that `println!` converts into a panic — visible to the user as
    // `failed printing to stdout: Broken pipe`. Resetting to `SIG_DFL`
    // makes the process die silently by signal when the downstream
    // reader goes away, matching what every other Unix tool does.
    // SAFETY: called once before any other thread exists.
    unsafe {
        let _ = nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGPIPE,
            nix::sys::signal::SigHandler::SigDfl,
        );
    }

    let cli = cli::Cli::parse();

    let mode = if cli.json {
        OutputMode::Json
    } else {
        OutputMode::Text
    };

    // Initialize logging
    let filter = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter)),
        )
        .with_target(false)
        .init();

    if let Err(e) = run(cli, mode) {
        // Commands that want a specific non-zero exit code without
        // an error message (restore refusal, check with errors)
        // return a `SilentExit` through the normal error channel
        // so `run()`'s cleanup (unmounting the btrfs toplevel)
        // still runs before the process exits.
        if let Some(silent) = e.downcast_ref::<SilentExit>() {
            process::exit(silent.0);
        }
        match mode {
            OutputMode::Json => output::emit_json_error(&format!("{e:#}")),
            OutputMode::Text => eprintln!("error: {e:#}"),
        }
        process::exit(1);
    }
}

/// Sentinel error used to request a non-zero exit code from `main`
/// without printing an error message. Propagates through `run()`
/// like any other error, which guarantees that `unmount_toplevel`
/// still runs before the process exits.
#[derive(Debug)]
struct SilentExit(i32);

impl std::fmt::Display for SilentExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "silent exit {}", self.0)
    }
}

impl std::error::Error for SilentExit {}

fn run(cli: cli::Cli, mode: OutputMode) -> Result<()> {
    // Root check
    if !nix::unistd::geteuid().is_root() {
        bail!("revenantctl requires root privileges");
    }

    // Handle init command before loading config (config may not exist yet)
    if let cli::Command::Init {
        output,
        force,
        systemd,
        systemd_dir,
        bin_path,
        timer_interval,
        periodic_strain,
        boot_strain,
        pacman,
        pacman_dir,
        pacman_strain,
    } = cli.command
    {
        return cmd_init(
            mode,
            InitArgs {
                output_path: output,
                force,
                systemd,
                systemd_dir,
                bin_path,
                timer_interval,
                periodic_strain,
                boot_strain,
                pacman,
                pacman_dir,
                pacman_strain,
            },
        );
    }

    // Handle check command separately so it can report a missing/invalid
    // config file as a finding instead of crashing during Config::load.
    if matches!(cli.command, cli::Command::Check) {
        return cmd_check(mode, Path::new(&cli.config));
    }

    let config = Config::load(Path::new(&cli.config)).context("failed to load configuration")?;

    let backend = BtrfsBackend::new();

    // Mount the btrfs top-level subvolume (subvolid=5) to a temporary location
    let toplevel = mount_toplevel(&config)?;

    let result = match cli.command {
        cli::Command::Snapshot {
            strain,
            message,
            trigger,
            trigger_unit,
        } => cmd_snapshot(
            mode,
            &config,
            &backend,
            &toplevel,
            &strain,
            message,
            trigger,
            trigger_unit,
        ),
        cli::Command::List { strain } => {
            cmd_list(mode, &config, &backend, &toplevel, strain.as_deref())
        }
        cli::Command::Restore {
            snapshot_id,
            strain,
            yes,
            force,
            save_current,
        } => cmd_restore(
            mode,
            &config,
            &backend,
            &toplevel,
            &snapshot_id,
            strain.as_deref(),
            yes,
            force,
            save_current,
        ),
        cli::Command::Delete {
            snapshot_id,
            strain,
            all,
        } => cmd_delete(
            mode,
            &config,
            &backend,
            &toplevel,
            snapshot_id.as_deref(),
            strain.as_deref(),
            all,
        ),
        cli::Command::Cleanup { dry_run } => {
            cmd_cleanup(mode, &config, &backend, &toplevel, dry_run)
        }
        cli::Command::Status => cmd_status(mode, &config, &backend, &toplevel),
        cli::Command::Init { .. } | cli::Command::Check => unreachable!(),
    };

    // Unmount toplevel
    unmount_toplevel(&toplevel);

    result
}

fn mount_toplevel(config: &Config) -> Result<std::path::PathBuf> {
    let mount_point = std::env::temp_dir().join("revenant-toplevel");
    std::fs::create_dir_all(&mount_point).context("failed to create temporary mount point")?;

    // Self-heal after a previous invocation that died before the
    // matching `unmount_toplevel` ran (e.g. SIGPIPE from a broken
    // stdout pipe). A leftover mount at our temp location would
    // otherwise make the btrfs `mount` below fail with EBUSY.
    if is_mount_point(&mount_point) {
        tracing::warn!(
            "stale mount at {} from a previous run; unmounting",
            mount_point.display()
        );
        if let Err(e) = nix::mount::umount(&mount_point) {
            tracing::warn!("failed to unmount stale {}: {e}", mount_point.display());
        }
    }

    let device = format!("/dev/disk/by-uuid/{}", config.sys.rootfs.device_uuid);

    nix::mount::mount(
        Some(device.as_str()),
        &mount_point,
        Some("btrfs"),
        nix::mount::MsFlags::empty(),
        Some("subvolid=5"),
    )
    .context("failed to mount btrfs top-level subvolume")?;

    tracing::debug!("mounted top-level at {}", mount_point.display());
    Ok(mount_point)
}

/// Return `true` when `path` sits on a different filesystem than its
/// parent directory — the classic stat-based mount-point check. Any
/// stat error (missing parent, missing path) is treated as "not a
/// mount point" so the caller falls through to a normal mount attempt.
fn is_mount_point(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else {
        return false;
    };
    let (Ok(self_meta), Ok(parent_meta)) = (std::fs::metadata(path), std::fs::metadata(parent))
    else {
        return false;
    };
    self_meta.dev() != parent_meta.dev()
}

fn unmount_toplevel(mount_point: &Path) {
    if let Err(e) = nix::mount::umount(mount_point) {
        tracing::warn!("failed to unmount {}: {e}", mount_point.display());
    }
    if let Err(e) = std::fs::remove_dir(mount_point) {
        tracing::debug!(
            "failed to remove mount point {}: {e}",
            mount_point.display()
        );
    }
}

/// Run the orphan-recovery hook before any state-modifying command.
///
/// Picks up nested subvolumes that ended up stranded in a DELETE
/// marker — typically a previous `restore` was interrupted between
/// the rename of `@` and the re-attach loop.  Failures inside the
/// hook are reported as warnings only; the surrounding command
/// proceeds either way.  Read-only commands (`list`, `status`,
/// `check`) intentionally do *not* call this so a stuck recovery
/// does not produce noise on every shell prompt.
///
/// In JSON mode the recovery notice is routed to stderr so it does
/// not pollute stdout, which is reserved for the machine-readable
/// payload.
fn recover_pending_orphans(
    mode: OutputMode,
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<()> {
    let recovered =
        revenant_core::cleanup::recover_orphaned_nested_subvols(config, backend, toplevel)
            .context("orphan recovery failed")?;
    if recovered > 0 {
        let msg = format!(
            "Recovered {recovered} orphaned nested subvolume(s) from a previous interrupted restore."
        );
        match mode {
            OutputMode::Text => println!("{msg}"),
            OutputMode::Json => eprintln!("{msg}"),
        }
    }
    Ok(())
}

/// Convert the CLI-surface trigger flags into a core `Trigger`.
///
/// For `--trigger pacman` we also read the package targets from stdin,
/// which is what the pacman PreTransaction hook feeds us when the hook
/// is generated with `NeedsTargets` (see `pkgmgr::pacman`). A read
/// failure does not fail the snapshot: we just record an empty target
/// list and let the user see what we got.
fn build_trigger(
    trigger: Option<cli::TriggerKindArg>,
    trigger_unit: Option<String>,
) -> revenant_core::metadata::Trigger {
    use revenant_core::metadata::{Trigger, TriggerKind};

    let kind: TriggerKind = trigger.map_or(TriggerKind::Manual, Into::into);

    match kind {
        TriggerKind::Manual | TriggerKind::Unknown => Trigger {
            kind,
            ..Trigger::default()
        },
        TriggerKind::Pacman => {
            let targets = read_stdin_lines().unwrap_or_else(|e| {
                tracing::warn!("failed to read pacman targets from stdin: {e}");
                Vec::new()
            });
            Trigger::pacman(targets)
        }
        TriggerKind::SystemdBoot | TriggerKind::SystemdPeriodic => {
            Trigger::systemd(kind, trigger_unit)
        }
        // Restore is created internally by the --save-current path and
        // is not selectable via the CLI `--trigger` flag (see
        // `cli::TriggerKindArg`), so this arm is unreachable in practice.
        TriggerKind::Restore => unreachable!("Restore trigger is not CLI-selectable"),
    }
}

/// Read stdin as a list of non-empty, trimmed lines.
///
/// Returns an empty vector immediately if stdin is attached to a
/// terminal: a `revenantctl snapshot --trigger pacman` invoked by a
/// human would otherwise block on `read` until the user manually sent
/// EOF. The pacman PreTransaction hook always pipes the target list in,
/// so it never hits this branch.
fn read_stdin_lines() -> std::io::Result<Vec<String>> {
    use std::io::{BufRead, IsTerminal};
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn cmd_snapshot(
    mode: OutputMode,
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    strain: &str,
    message: Option<String>,
    trigger: Option<cli::TriggerKindArg>,
    trigger_unit: Option<String>,
) -> Result<()> {
    recover_pending_orphans(mode, config, backend, toplevel)?;

    let trigger = build_trigger(trigger, trigger_unit);

    let info = snapshot::create_snapshot(config, backend, toplevel, strain, message, trigger)
        .context("failed to create snapshot")?;

    // Discover once, use for retention
    let snapshots = snapshot::discover_snapshots(config, backend, toplevel)
        .context("failed to discover snapshots")?;
    let retention_removed =
        revenant_core::cleanup::apply_retention_to(config, backend, toplevel, &snapshots)
            .context("retention cleanup failed")?;

    output::print_snapshot_created(mode, &info, &retention_removed);
    Ok(())
}

fn cmd_list(
    mode: OutputMode,
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    strain_filter: Option<&str>,
) -> Result<()> {
    let snapshots = snapshot::discover_snapshots(config, backend, toplevel)
        .context("failed to discover snapshots")?;
    let filtered: Vec<_> = match strain_filter {
        Some(strain) => snapshots
            .into_iter()
            .filter(|s| s.strain == strain)
            .collect(),
        None => snapshots,
    };
    let live_parent = snapshot::resolve_live_parent(config, backend, toplevel);
    output::print_snapshot_list(mode, &filtered, live_parent.as_ref());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_restore(
    mode: OutputMode,
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    id_str: &str,
    strain: Option<&str>,
    yes: bool,
    force: bool,
    save_current: bool,
) -> Result<()> {
    recover_pending_orphans(mode, config, backend, toplevel)?;

    let id: SnapshotId = id_str.parse().context("invalid snapshot ID format")?;

    let snap = snapshot::find_snapshot(config, backend, toplevel, &id, strain)
        .context("failed to find snapshot")?;

    // Pre-flight gates run before the dry-run / refusal branch so a user
    // who runs `restore` without `--yes` still sees blocking conditions
    // up front, not only on the second invocation. `--force` overrides
    // Error-severity findings; `--yes` does not (the two flags gate
    // distinct concerns).
    let preflight_findings =
        preflight::preflight_restore(Path::new(preflight::MACHINED_RUNTIME_DIR));
    if !preflight_findings.is_empty() {
        output::print_findings(mode, &preflight_findings);
    }
    let has_blocking = preflight_findings
        .iter()
        .any(|f| f.severity == Severity::Error);
    if has_blocking && !force {
        return Err(SilentExit(1).into());
    }

    if !yes {
        // Refusal path: explain what would happen and exit with code 1.
        // This is deliberately not an interactive prompt — passing --yes
        // is the user's explicit acknowledgement that they understand
        // the consequences.
        //
        // Return `SilentExit` rather than calling `process::exit`
        // directly so the toplevel btrfs mount set up in `run()` is
        // still unmounted on the way out. Otherwise a subsequent
        // `restore --yes` would fail with `EBUSY` on the re-mount.
        let strain_config = config.strain(&snap.strain)?;
        let efi_sync = snap.efi_synced && config.sys.efi.enabled;
        output::print_restore_refusal(
            mode,
            &snap,
            &strain_config.subvolumes,
            efi_sync,
            &config.sys.efi.mount_point,
        );
        return Err(SilentExit(1).into());
    }

    // Take a retained snapshot of the current state first when requested.
    // If this step fails, we abort before touching any live subvolume —
    // the whole point of --save-current is a safety net, so a silent
    // proceed-without-snapshot would defeat the feature.
    let pre_restore = if save_current {
        use revenant_core::metadata::Trigger;
        let info = snapshot::create_snapshot(
            config,
            backend,
            toplevel,
            &snap.strain,
            None,
            Trigger::restore(snap.id.to_string()),
        )
        .context("failed to create pre-restore snapshot; restore aborted")?;
        let all = snapshot::discover_snapshots(config, backend, toplevel)
            .context("failed to discover snapshots for retention")?;
        // Retention output is discarded here — the pre-restore snapshot
        // and the removal of older ones are both reported via the
        // restore output struct downstream.
        let _ = revenant_core::cleanup::apply_retention_to(config, backend, toplevel, &all)
            .context("retention cleanup after pre-restore snapshot failed")?;
        Some(info)
    } else {
        None
    };

    revenant_core::restore::restore_snapshot(config, backend, toplevel, &snap)
        .context("restore failed")?;

    output::print_restore_success(mode, &snap, pre_restore.as_ref());
    Ok(())
}

fn cmd_delete(
    mode: OutputMode,
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    id_str: Option<&str>,
    strain: Option<&str>,
    all: bool,
) -> Result<()> {
    recover_pending_orphans(mode, config, backend, toplevel)?;

    if all {
        let strain_name = strain.ok_or_else(|| anyhow::anyhow!("--all requires --strain"))?;
        let removed = snapshot::delete_all_strain(config, backend, toplevel, strain_name)
            .context("failed to delete strain snapshots")?;
        output::print_delete_result(mode, strain_name, &removed, false);
    } else {
        let id_str = id_str.ok_or_else(|| {
            anyhow::anyhow!("snapshot ID is required (or use --strain with --all)")
        })?;
        let id: SnapshotId = id_str.parse().context("invalid snapshot ID format")?;

        let snap = snapshot::find_snapshot(config, backend, toplevel, &id, strain)
            .context("failed to find snapshot")?;
        let snap_strain = snap.strain.clone();

        snapshot::delete_snapshot(config, backend, toplevel, &snap)
            .context("failed to delete snapshot")?;

        output::print_delete_result(mode, &snap_strain, &[id.to_string()], true);
    }

    Ok(())
}

fn cmd_cleanup(
    mode: OutputMode,
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        let plan = revenant_core::cleanup::plan_retention(config, backend, toplevel)
            .context("cleanup dry-run failed")?;
        output::print_retention_plan(mode, &plan);
        return Ok(());
    }

    recover_pending_orphans(mode, config, backend, toplevel)?;

    let summary = revenant_core::cleanup::apply_retention(config, backend, toplevel)
        .context("cleanup failed")?;

    output::print_cleanup_result(mode, &summary);

    Ok(())
}

fn cmd_status(
    mode: OutputMode,
    config: &Config,
    backend: &dyn FileSystemBackend,
    toplevel: &Path,
) -> Result<()> {
    let snapshots = snapshot::discover_snapshots(config, backend, toplevel)
        .context("failed to discover snapshots")?;
    output::print_status(mode, config, &snapshots);
    Ok(())
}

fn cmd_check(mode: OutputMode, config_path: &Path) -> Result<()> {
    let mut findings: Vec<Finding> = Vec::new();

    // Step 1: config file check (does not require a loaded Config)
    let config_findings = check::check_config_file(config_path);
    let config_ok = config_findings.is_empty();
    findings.extend(config_findings);

    // Steps 2+: only run if config is usable
    if config_ok {
        let config = Config::load(config_path).context("failed to load configuration")?;
        let backend = BtrfsBackend::new();
        let toplevel = mount_toplevel(&config)?;

        let orphans = check::find_orphaned_snapshots(&config, &backend, &toplevel);
        let orphan_sidecars = check::find_orphaned_sidecars(&config, &backend, &toplevel);
        let nested = check::find_nested_subvolumes(&config, &backend, &toplevel);

        unmount_toplevel(&toplevel);

        match orphans {
            Ok(f) => findings.extend(f),
            Err(e) => findings.push(Finding::error(
                "orphaned-snapshot",
                format!("scan failed: {e}"),
            )),
        }
        match orphan_sidecars {
            Ok(f) => findings.extend(f),
            Err(e) => findings.push(Finding::error(
                "orphaned-sidecar",
                format!("scan failed: {e}"),
            )),
        }
        findings.extend(nested);
    }

    output::print_findings(mode, &findings);

    let errors = findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    if errors > 0 {
        return Err(SilentExit(1).into());
    }
    Ok(())
}

struct InitArgs {
    output_path: String,
    force: bool,
    systemd: bool,
    systemd_dir: String,
    bin_path: Option<String>,
    timer_interval: String,
    periodic_strain: String,
    boot_strain: String,
    pacman: bool,
    pacman_dir: String,
    pacman_strain: String,
}

fn cmd_init(mode: OutputMode, args: InitArgs) -> Result<()> {
    let InitArgs {
        output_path,
        force,
        systemd,
        systemd_dir,
        bin_path,
        timer_interval,
        periodic_strain,
        boot_strain,
        pacman,
        pacman_dir,
        pacman_strain,
    } = args;

    let mut reporter = InitReporter::new(mode);
    let config_path = Path::new(&output_path);
    let config_exists = config_path.exists();

    // Three modes:
    //   1. Fresh / --force           → detect system, build config from
    //                                  scratch, add systemd / pacman strains
    //                                  if asked.
    //   2. Existing + integration    → load config, add any missing strains
    //                                  required by --systemd or --pacman,
    //                                  write back. User customisations on
    //                                  other strains are preserved.
    //   3. Existing + plain `init`   → bail (would clobber).
    if !config_exists || force {
        let detected = revenant_core::init::detect_all().context("system detection failed")?;

        reporter.task(InitTask::DetectedSystem {
            backend: detected.backend.to_string(),
            device_uuid: detected.device_uuid.clone(),
            rootfs_subvol: detected.rootfs_subvol.clone(),
            snapshot_subvol: "@snapshots".to_string(),
            efi_mount: detected.efi.as_ref().map(|e| e.mount_point.clone()),
            bootloader: detected.bootloader.clone(),
        });

        let mut config = revenant_core::init::build_config(detected);
        if systemd {
            let added = ensure_systemd_strains(&mut config, &boot_strain, &periodic_strain);
            if !added.is_empty() {
                // In the fresh path the added strains are part of the
                // config that's about to be written; surface them as a
                // dedicated task entry so JSON consumers see them
                // before `wrote-config`.
                reporter.task(InitTask::AddedSystemdStrains { strains: added });
            }
        }
        if pacman && ensure_pacman_strain(&mut config, &pacman_strain) {
            reporter.task(InitTask::AddedPkgmgrStrain {
                pm: pkgmgr::pacman::Pacman.name().to_string(),
                strain: pacman_strain.clone(),
            });
        }

        write_config_to(&config, config_path, &output_path)?;
        reporter.task(InitTask::WroteConfig {
            path: output_path.clone(),
            created: !config_exists,
        });
    } else if systemd || pacman {
        let mut config = revenant_core::Config::load(config_path)
            .context("failed to load existing configuration")?;
        let mut touched = false;

        if systemd {
            let added = ensure_systemd_strains(&mut config, &boot_strain, &periodic_strain);
            if !added.is_empty() {
                reporter.task(InitTask::AddedSystemdStrains { strains: added });
                touched = true;
            }
        }
        if pacman && ensure_pacman_strain(&mut config, &pacman_strain) {
            reporter.task(InitTask::AddedPkgmgrStrain {
                pm: pkgmgr::pacman::Pacman.name().to_string(),
                strain: pacman_strain.clone(),
            });
            touched = true;
        }

        if touched {
            write_config_to(&config, config_path, &output_path)?;
            reporter.task(InitTask::WroteConfig {
                path: output_path.clone(),
                created: false,
            });
        }
    } else {
        bail!("configuration file already exists: {output_path}\nUse --force to overwrite.");
    }

    // Resolve the revenantctl binary path once — both writers need it.
    let resolved_bin = if systemd || pacman {
        Some(resolve_bin_path(bin_path.as_deref())?)
    } else {
        None
    };

    if systemd {
        write_systemd_units(
            &mut reporter,
            SystemdUnitArgs {
                config_path: output_path.clone(),
                force,
                systemd_dir,
                bin_path: resolved_bin.clone().expect("resolved above"),
                timer_interval,
                periodic_strain,
                boot_strain,
            },
        )?;
    }

    if pacman {
        let pm = pkgmgr::pacman::Pacman;
        write_pkgmgr_hooks(
            &mut reporter,
            &pm,
            PkgmgrHookArgs {
                config_path: output_path,
                force,
                hook_dir: pacman_dir,
                bin_path: resolved_bin.expect("resolved above"),
                strain: pacman_strain,
            },
        )?;
    }

    reporter.finish();
    Ok(())
}

/// Resolve the `revenantctl` binary path used by generated hook files.
/// If the user passed `--bin-path`, honour it verbatim; otherwise fall
/// back to the canonicalised path of the currently running executable.
fn resolve_bin_path(bin_path: Option<&str>) -> Result<std::path::PathBuf> {
    match bin_path {
        Some(p) => Ok(std::path::PathBuf::from(p)),
        None => std::env::current_exe()
            .context("failed to detect binary path")?
            .canonicalize()
            .context("failed to resolve binary path"),
    }
}

/// Insert the boot and periodic strains into `config` if they are not
/// already defined. Returns the names of the strains that were actually
/// added (in insertion order), so the caller can report what changed.
///
/// Pre-existing strains with the same name are left untouched — the user
/// may have customised them and we must not silently overwrite their work.
fn ensure_systemd_strains(
    config: &mut revenant_core::Config,
    boot_strain: &str,
    periodic_strain: &str,
) -> Vec<String> {
    let mut added = Vec::new();
    let efi_enabled = config.sys.efi.enabled;

    if !config.strain.contains_key(boot_strain) {
        config.strain.insert(
            boot_strain.to_string(),
            revenant_core::StrainConfig {
                retain: revenant_core::RetainConfig {
                    last: 5,
                    ..Default::default()
                },
                subvolumes: vec![config.sys.rootfs_subvol.clone()],
                efi: efi_enabled,
            },
        );
        added.push(boot_strain.to_string());
    }

    if !config.strain.contains_key(periodic_strain) {
        config.strain.insert(
            periodic_strain.to_string(),
            revenant_core::StrainConfig {
                retain: revenant_core::RetainConfig {
                    last: 5,
                    hourly: 48,
                    daily: 14,
                    ..Default::default()
                },
                subvolumes: vec![config.sys.rootfs_subvol.clone()],
                efi: efi_enabled,
            },
        );
        added.push(periodic_strain.to_string());
    }

    added
}

/// Insert the pacman strain into `config` if it does not already exist.
/// Returns `true` when a new strain was inserted, `false` when one with
/// the same name was already present (we never overwrite user tweaks).
///
/// The retention default of `last = 10` keeps roughly ten transactions
/// worth of undo history — enough to cover a typical upgrade session
/// without ballooning disk usage.
fn ensure_pacman_strain(config: &mut revenant_core::Config, pacman_strain: &str) -> bool {
    if config.strain.contains_key(pacman_strain) {
        return false;
    }
    let efi_enabled = config.sys.efi.enabled;
    config.strain.insert(
        pacman_strain.to_string(),
        revenant_core::StrainConfig {
            retain: revenant_core::RetainConfig {
                last: 10,
                ..Default::default()
            },
            subvolumes: vec![config.sys.rootfs_subvol.clone()],
            efi: efi_enabled,
        },
    );
    true
}

/// Serialize `config` to TOML and write it to `config_path`, creating
/// the parent directory if needed.  `output_path` is only used to make
/// error messages refer to the user-supplied path.
fn write_config_to(
    config: &revenant_core::Config,
    config_path: &Path,
    output_path: &str,
) -> Result<()> {
    let toml_str =
        revenant_core::init::config_to_toml(config).context("failed to serialize config")?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).context("failed to create config directory")?;
    }
    std::fs::write(config_path, &toml_str)
        .with_context(|| format!("failed to write {output_path}"))?;
    Ok(())
}

struct SystemdUnitArgs {
    config_path: String,
    force: bool,
    systemd_dir: String,
    bin_path: std::path::PathBuf,
    timer_interval: String,
    periodic_strain: String,
    boot_strain: String,
}

fn write_systemd_units(reporter: &mut InitReporter, args: SystemdUnitArgs) -> Result<()> {
    let SystemdUnitArgs {
        config_path,
        force,
        systemd_dir,
        bin_path,
        timer_interval,
        periodic_strain,
        boot_strain,
    } = args;

    let params = revenant_core::systemd::SystemdUnitParams {
        bin_path,
        config_path: std::path::PathBuf::from(&config_path),
        boot_strain,
        periodic_strain,
        timer_calendar: timer_interval,
    };

    let units = revenant_core::systemd::generate_units(&params);
    let dir = Path::new(&systemd_dir);

    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create directory {systemd_dir}"))?;

    reporter.systemd_header(&systemd_dir);
    for unit in &units {
        let unit_path = dir.join(&unit.filename);
        let mut status = "written";
        if unit_path.exists() {
            // Idempotent re-run: if the on-disk content already matches
            // what we'd write, leave the file alone.  Only bail when the
            // user has actually customised it (or it differs for some
            // other reason) and didn't pass --force.
            let existing = std::fs::read(&unit_path).ok();
            if existing.as_deref() == Some(unit.content.as_bytes()) {
                reporter.task(InitTask::WroteSystemdUnit {
                    name: unit.filename.clone(),
                    status: "unchanged".to_string(),
                });
                continue;
            }
            if !force {
                bail!(
                    "unit file already exists with different content: {}\nUse --force to overwrite.",
                    unit_path.display()
                );
            }
            status = "overwritten";
        }
        std::fs::write(&unit_path, &unit.content)
            .with_context(|| format!("failed to write {}", unit_path.display()))?;
        reporter.task(InitTask::WroteSystemdUnit {
            name: unit.filename.clone(),
            status: status.to_string(),
        });
    }

    reporter.systemd_footer();
    Ok(())
}

struct PkgmgrHookArgs {
    config_path: String,
    force: bool,
    hook_dir: String,
    bin_path: std::path::PathBuf,
    strain: String,
}

/// Write the hook files produced by a [`PackageManager`] to disk with
/// the same idempotent semantics as `write_systemd_units`: unchanged
/// files are left alone, differing files require `--force` to overwrite.
fn write_pkgmgr_hooks(
    reporter: &mut InitReporter,
    pm: &dyn PackageManager,
    args: PkgmgrHookArgs,
) -> Result<()> {
    let PkgmgrHookArgs {
        config_path,
        force,
        hook_dir,
        bin_path,
        strain,
    } = args;

    let params = pkgmgr::HookParams {
        bin_path,
        config_path: std::path::PathBuf::from(&config_path),
        strain,
    };

    let hooks = pm.generate_hooks(&params);
    let dir = Path::new(&hook_dir);

    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create directory {hook_dir}"))?;

    reporter.pkgmgr_header(pm.name(), &hook_dir);
    for hook in &hooks {
        let hook_path = dir.join(&hook.filename);
        let mut status = "written";
        if hook_path.exists() {
            let existing = std::fs::read(&hook_path).ok();
            if existing.as_deref() == Some(hook.content.as_bytes()) {
                reporter.task(InitTask::WrotePkgmgrHook {
                    pm: pm.name().to_string(),
                    name: hook.filename.clone(),
                    status: "unchanged".to_string(),
                });
                continue;
            }
            if !force {
                bail!(
                    "hook file already exists with different content: {}\nUse --force to overwrite.",
                    hook_path.display()
                );
            }
            status = "overwritten";
        }
        std::fs::write(&hook_path, &hook.content)
            .with_context(|| format!("failed to write {}", hook_path.display()))?;
        reporter.task(InitTask::WrotePkgmgrHook {
            pm: pm.name().to_string(),
            name: hook.filename.clone(),
            status: status.to_string(),
        });
    }

    reporter.pkgmgr_footer(pm.name());
    Ok(())
}
