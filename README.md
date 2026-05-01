> [!WARNING]
> **Beta software.** Revenant has been in daily use on a handful of systems for
> several weeks without an incident, and is no longer "early development". That
> said, it is a tool that renames live subvolumes and rewrites ESP contents —
> bugs at this stage can still leave a system unbootable or destroy data. Treat
> it as beta software on a production system and **always keep an independent
> backup**. If you would rather not take that risk, run revenant in a VM until
> tagged releases stabilise the on-disk layout and the public APIs.

# revenant

A system snapshot tool for Linux, written in Rust. Inspired by
[Timeshift](https://github.com/linuxmint/timeshift), but focused on CoW filesystems with
systemd-boot integration and the EFI partition snapshotted alongside the rootfs.

## Motivation

Timeshift works, but it comes with opinions: it assumes GRUB, it ignores your EFI partition,
it expects Ubuntu-style subvolume layouts. If your system uses systemd-boot, a non-standard
Btrfs layout, and you actually want your bootloader files to be part of the snapshot, Timeshift
leaves you on your own.

Revenant was built to fill that gap. It works with any bootloader, but the EFI backup is
most valuable with systemd-boot, where kernel images and boot entries live on the ESP.
GRUB-based setups typically keep kernels in `/boot` on the root filesystem (already covered
by the btrfs snapshot), so the EFI sync adds little there — but revenant's core snapshot and
restore functionality works just the same.

An EFI partition is **not** required. On BIOS systems, or any setup where `/boot` is a
directory inside the rootfs, set `sys.efi.enabled = false` in the config and revenant
operates as a pure rootfs snapshot tool. `revenantctl init` detects this automatically.

## How it compares

Only the rows where the three tools actually differ — all of them can take snapshots,
apply retention, and restore, so those are omitted. Third-party add-ons like `grub-btrfs`,
`snap-pac`, `btrfs-assistant` or `snapper-gui` are deliberately **not** counted toward
Timeshift or snapper, because they are not part of the respective upstream projects.

| Feature | revenant | Timeshift | snapper |
|---|---|---|---|
| Filesystem backends | btrfs (ZFS/bcachefs planned) | btrfs, rsync/ext4 | btrfs, ext4, LVM-thin |
| **EFI partition snapshotted in sync with rootfs** | ✓ | ✗ | ✗ |
| Bootloader integration | any (EFI sync most useful with systemd-boot) | GRUB2 (reinstalled on restore) | — (rollback just flips `btrfs default subvol`) |
| Independent snapshot profiles with own retention | ✓ (strains) | ✗ (fixed Hourly/Daily/Weekly/Monthly/Boot) | ✓ (per-subvolume configs) |
| Package-manager pre/post hooks (upstream) | ✓ pacman (`init --pacman`); apt/zypp planned | — | zypp (openSUSE) |
| JSON / scriptable CLI | ✓ `--json` | ✗ | ✓ `--jsonout` |
| Upstream GUI | ✓ (GTK4 / libadwaita, talks to a privileged D-Bus daemon) | GTK | — |

Sources: [Timeshift README](https://github.com/linuxmint/timeshift/blob/master/README.md),
[`snapper(8)`](https://github.com/openSUSE/snapper/blob/master/doc/snapper.xml.in),
[`snapper-configs(5)`](https://github.com/openSUSE/snapper/blob/master/doc/snapper-configs.xml.in).

## What it does

Revenant creates atomic, point-in-time snapshots of a Btrfs system and restores them cleanly.
The key difference from other tools: **the EFI partition is backed up too**, so a snapshot
and the corresponding ESP state are always kept together. This is especially valuable with
systemd-boot, where kernels and boot entries live on the ESP. A restore is rejected if the
EFI snapshot for a given ID is missing. On systems without an EFI partition, EFI sync can
be disabled entirely and revenant runs as a rootfs-only snapshot tool.

### Snapshot naming

Snapshots follow the pattern `{subvol}-{strain}-{timestamp}`, e.g.
`@-default-20260316-143022-456`. The timestamp is `YYYYMMDD-HHMMSS-mmm` UTC
with a millisecond suffix; older 15-character IDs without the suffix
remain readable. The subvolume name carries the snapshot's identity —
strain, exact creation moment, and source subvolume — so there is no
central snapshot database. Optional per-snapshot metadata is stored
next to the subvolume as a TOML sidecar (see below) and is strictly
additive.

### Snapshot metadata

Alongside each snapshot, revenant writes an optional TOML sidecar
`{strain}-{timestamp}.meta.toml` in the snapshot directory (e.g.
`@snapshots/`, next to the snapshot subvolumes themselves) that
records *why* the snapshot exists:

- A free-form `--message` supplied by the user on manual snapshots.
- The trigger kind (`manual`, `pacman`, `systemd-boot`, `systemd-periodic`,
  `restore`).
- For pacman-triggered snapshots, the list of target packages read from
  the hook's stdin.
- For systemd-triggered snapshots, the unit that fired them.
- For `restore`-triggered snapshots (created by `restore --save-current`
  before the rollback), the id of the snapshot the restore was heading to.
- A local-time `created_at` timestamp (so the sidecar is readable on its own,
  independent of the UTC id in the subvolume name).

Sidecars are strictly additive: snapshots without a sidecar (legacy or
externally created) list fine and restore normally. A failure to write the
sidecar never fails the snapshot itself — metadata loss is preferable to a
stranded half-created snapshot.

`list` and `snapshot` surface the metadata in both text and JSON output;
`check` flags sidecars whose snapshot subvolume has disappeared as
`orphaned-sidecar`, and `cleanup` removes them.

### Strains

A *strain* is a named snapshot namespace with its own configuration: which subvolumes to
snapshot, whether to include the EFI partition, and how many snapshots to retain. You can
define multiple strains for different purposes (e.g. `default` for manual snapshots,
`pacman` triggered before package upgrades).

### Retention

Each strain defines its own retention policy (`last`, `hourly`, `daily`, …).
By default retention is applied automatically every time a new snapshot
is created (`sys.auto_apply_retention = true`). Set it to `false` if you
want snapshot creation to never delete anything; retention then runs
only when you invoke `revenantctl cleanup` explicitly.

`revenantctl cleanup` does three things in one pass: apply per-strain
retention, expire DELETE markers older than `sys.tombstone_max_age_days`
(default 14 days; see *Restore and the DELETE marker* below), and sweep
up orphaned metadata sidecars whose snapshot subvolume is gone. Pass
`--dry-run` to preview the plan, or `--force` to skip the DELETE-marker
undo window and purge every marker right away (per-strain retention is
still honoured — `--force` only bypasses the tombstone cooldown).

### Restore and the DELETE marker

`revenantctl restore <id>` is destructive and refuses to run without an explicit
`--yes`. Without the flag it prints what it would do and exits with code 1; this
makes the user's acknowledgement a single explicit step without requiring an
interactive prompt (so it is still script-friendly via `--yes`).

A restore renames the current live subvolume to `{subvol}-DELETE-{ts}` and
creates a fresh writable copy from the chosen snapshot in its place. The
renamed subvolume is **not** purged immediately: it lives at the top level of
the btrfs filesystem as a volatile undo buffer for the previous live state. If
you need a file from the pre-restore system after rebooting, it is still
there, writable, until it ages out.

DELETE markers expire automatically `sys.tombstone_max_age_days` days after
they were created (default 14 days); the next retention pass — whether
triggered by `revenantctl snapshot` with `auto_apply_retention = true` or by
an explicit `revenantctl cleanup` — drops every DELETE marker older than
that. Recent markers survive the routine cleanup, so the undo buffer is
actually available when you need it. To skip the cooldown and purge every
marker now, run `revenantctl cleanup --force`. Setting
`sys.tombstone_max_age_days = 0` disables auto-expiry entirely; markers then
stay until you remove them explicitly (`cleanup --force`, or the GUI's
review dialog).

If you want a retained, strain-integrated copy of the current state instead
of (or in addition to) the volatile DELETE marker, pass `--save-current` to
`restore`: it captures the pre-restore state as a snapshot in the target
strain (tagged with the `restore` trigger kind) just before the rollback, so
you have a named, retention-managed point to return to.

After a restore, `revenantctl list` marks the snapshot that the current live
subvolume was cloned from with a leading `*`, so the rollback anchor is always
visible in the snapshot listing. In JSON mode the same information is exposed
as an optional top-level `live_parent` field alongside `snapshots`.

### EFI backup strategy

The EFI partition is not a Btrfs subvolume, so revenant maintains a staging subvolume (e.g.
`@boot`) on the Btrfs filesystem. Before each snapshot, the EFI partition contents are
rsync-like-copied into the staging subvolume (block-level, inplace, only changed blocks), then
a read-only snapshot of that staging subvolume is taken with the same ID as the root snapshot.
On restore, both snapshots must be present for the ID.

## Current state

- [x] Btrfs backend via direct ioctls (no external `btrfs` binary required)
- [x] Bootloader-agnostic (EFI sync optimised for systemd-boot)
- [x] EFI partition backup and restore
- [x] Snapshot creation, listing, deletion
- [x] Retention policy / cleanup
- [x] `revenantctl init` — auto-detects system configuration from `/proc/self/mountinfo`
  and generates a ready-to-use `config.toml`
- [x] Multiple strains with independent retention settings
- [x] Restore flow with `--yes` confirmation and DELETE-marker undo buffer
- [x] Snapshots stored in dedicated `@snapshots` subvolume (configurable)
- [x] Per-snapshot metadata sidecars (`--message`, trigger context, package targets)
- [x] GUI (`revenant-gui`, GTK4 + libadwaita) backed by `revenant-daemon` (system-bus
      service, polkit-gated privileged operations)
- [x] `revenantctl init --systemd` — generates systemd units for boot and periodic snapshots
- [x] `revenantctl check` — health checks for config, orphaned snapshots and nested subvolumes
- [ ] ZFS / bcachefs backends (trait is defined, implementations pending)

## Architecture

Revenant is a Cargo workspace with four crates:

| Crate | Role |
|---|---|
| `revenant-core` | Library: all snapshot logic, backend trait, config, EFI sync |
| `revenant-cli` | Binary `revenantctl`: the command-line interface |
| `revenant-daemon` | Binary `revenantd`: system-bus D-Bus service, polkit-gated privileged operations |
| `revenant-gui` | Binary `revenant-gui`: GTK4 + libadwaita desktop client, talks to the daemon over D-Bus |

The CLI is standalone and does not depend on the daemon — `revenantctl` runs as root and
talks to the backend directly. The GUI is the unprivileged client of `revenant-daemon`,
which owns the btrfs toplevel mount and gates every write through polkit. The wire contract
is documented in [`crates/revenant-daemon/dbus-interface.md`](crates/revenant-daemon/dbus-interface.md).

The `FileSystemBackend` trait abstracts all COW filesystem operations, making it straightforward
to add ZFS or bcachefs backends later without touching the core logic.

No external binaries are required at runtime — all Btrfs operations go through ioctls directly.
This means revenant works even in a minimal recovery environment.

## Installation

### From source

```sh
cargo build --release --workspace
sudo install -Dm755 target/release/revenantctl /usr/local/bin/revenantctl
# Optional, only if you want the GUI + privileged daemon:
sudo install -Dm755 target/release/revenantd    /usr/local/bin/revenantd
sudo install -Dm755 target/release/revenant-gui /usr/local/bin/revenant-gui
```

The daemon and the GUI need their D-Bus and polkit policy files in place to actually
work; the recipe is in [`crates/revenant-daemon/README.md`](crates/revenant-daemon/README.md).

### Arch Linux packages

Each tagged release attaches two `*.pkg.tar.zst` files for `x86_64` to its GitHub
release page (no aarch64 packages — for ARM systems use the static `revenantctl`
musl binary from the same release page, or build the workspace from source).
Download the matching version and install with `pacman -U`:

```sh
sudo pacman -U revenant-<version>-1-x86_64.pkg.tar.zst
# Optional, depends on the revenant package:
sudo pacman -U revenant-gui-<version>-1-x86_64.pkg.tar.zst
```

`revenant` ships the CLI (`revenantctl`); `revenant-gui` ships the privileged daemon
(`revenantd`) plus the GTK4 client (`revenant-gui`) along with the D-Bus, polkit and
systemd policy files.

To build the same packages locally from a working tree:

```sh
cd packaging/arch
makepkg -fi      # build, then install with pacman -U behind the scenes
```

The PKGBUILD reads `pkgver` from the workspace `Cargo.toml`, so building from any
checkout produces a package matching that revision; no manual version edit needed.

## Usage

```
revenantctl [OPTIONS] <COMMAND>

Options:
  --config <PATH>   Configuration file [default: /etc/revenant/config.toml]
  -v, -vv, -vvv     Increase verbosity
  -j, --json        Emit machine-readable JSON on stdout (see below)

Commands:
  init      Auto-detect system and generate config file
  snapshot  Create a new snapshot
  list      List all snapshots (optionally filter by strain)
  restore   Restore a snapshot by ID
  delete    Delete a snapshot or all snapshots of a strain
  cleanup   Apply retention policy and remove old snapshots
  status    Show configuration and filesystem status
  check     Run system health checks
```

### JSON output

All commands accept a global `-j` / `--json` flag that switches stdout to a
single JSON document per invocation.  Tracing/log output continues to go to
stderr so that consumers can rely on clean stdout for `jq`, `python -m json.tool`,
or any other parser.

The rough shape per command is:

| Command | JSON payload |
|---|---|
| `list` | `{"snapshots": [{"id","strain","subvolumes","efi_synced","metadata"?}, …], "live_parent"?: {"id","strain"}}` |
| `status` | `{"config": {…}, "strain_snapshots": {name: count}, "snapshots_total": N}` |
| `snapshot` | `{"created": SnapshotInfo, "retention_removed": [id, …]}` |
| `delete` | `{"strain": "…", "deleted": [id, …]}` |
| `restore` (`--yes`) | `{"restored": {"id","strain"}, "pre_restore_snapshot"?: {"id","strain"}, "reboot_required": true}` |
| `restore` (refusal) | `{"would_restore": {…}, "subvolumes": […], "efi_sync": bool, "proceed_with": "--yes"}`, exit 1 |
| `cleanup` | `{"removed": [id, …], "removed_sidecars": [name, …]}` |
| `cleanup --dry-run` | the full `RetentionPlan` — per-strain keep/delete entries plus every DELETE marker, each tagged with `would_purge` and `expires_at` |
| `check` | `{"findings": [{"severity","check","message","hint"?}, …], "summary": {"errors","warnings","infos"}}` |
| `init` | `{"tasks": [{"task": "detected-system" \| "wrote-config" \| "added-systemd-strains" \| "wrote-systemd-unit" \| "added-pkgmgr-strain" \| "wrote-pkgmgr-hook", …}, …]}` |

Errors in JSON mode land as `{"error": "..."}` on stdout with a non-zero exit
code, so a script can consistently read from stdout and branch on exit status.
For `check` and `restore`, the exit code also encodes the outcome (non-zero on
any error finding / on the refusal path), matching the text-mode semantics.

> [!NOTE]
> **JSON schema is not yet stable.**  Until revenant reaches a 1.0 release,
> field names, task enum variants, and overall shapes may still change.
> Scripts that consume JSON output should pin to a specific revenant version.

### Quick start

```sh
# Detect your system and write /etc/revenant/config.toml
sudo revenantctl init

# Take a snapshot using the default strain
sudo revenantctl snapshot

# Take a snapshot with a descriptive message (recorded in the sidecar)
sudo revenantctl snapshot --message "before risky experiment"

# List all snapshots
revenantctl list

# Restore a specific snapshot (prints what would happen and exits with code 1)
sudo revenantctl restore 20260316-143022-456
# Re-run with --yes to actually perform the restore
sudo revenantctl restore 20260316-143022-456 --yes
```

### Systemd integration

Generate systemd units for automatic snapshots:

```sh
# Generate config + systemd units
sudo revenantctl init --systemd

# Enable boot snapshot (runs once after each boot)
sudo systemctl enable revenant-boot.service

# Enable periodic snapshots (hourly by default)
sudo systemctl enable --now revenant-periodic.timer
```

The timer interval and periodic strain name are configurable:

```sh
sudo revenantctl init --systemd --timer-interval "*-*-* 00/4:00:00" --periodic-strain hourly
```

### Pacman integration

On Arch-family systems, revenant can install a pacman `PreTransaction` hook
that snapshots the system before every package install, upgrade or removal:

```sh
sudo revenantctl init --pacman
```

This adds a `pacman` strain to the config (retain 10 by default) and writes
`/etc/pacman.d/hooks/50-revenant-snapshot.hook`. The hook is deliberately
non-blocking: if snapshotting fails for any reason, the transaction still
proceeds (revenant logs the error to stderr, where it surfaces in pacman's
output). A broken snapshot tool must never turn into a broken package
upgrade.

Because pacman acquires its database lock `/var/lib/pacman/db.lck` *before*
running `PreTransaction` hooks, every hook-triggered snapshot captures that
lock file. Revenant strips it from the restored tree during `restore` so
the first `pacman` invocation after a rollback does not abort with
`unable to lock database`. The cleanup is unconditional and covers every
package manager revenant knows about, so it is a no-op on systems that
never installed a pacman hook.

`--pacman` composes with `--systemd`, so a fresh box can be set up in a
single invocation:

```sh
sudo revenantctl init --systemd --pacman
```

Support for `apt` (Debian / Ubuntu) and `zypp` (openSUSE) is planned; the
package-manager backends sit behind a trait so adding them does not
require reshaping existing code.

### Health checks

`revenantctl check` runs a set of non-destructive checks against the current
system state and reports findings as warnings or errors. It exits non-zero if
any errors are found, so it can be used in scripts or monitoring.

Current checks:

- **config-missing / config-invalid** — verifies that the configuration file
  exists and parses. Reports a hint pointing to `revenantctl init` if not.
- **orphaned-snapshot** — scans the snapshot subvolume for entries matching
  the revenant naming scheme (`{subvol}-{strain}-{timestamp}`) that no
  configured strain claims. Useful for detecting leftovers from a strain that
  was removed from the config, or accidental snapshots created with a
  different config.
- **orphaned-sidecar** — sidecar metadata files (`*.meta.toml`) whose
  matching snapshot subvolume is gone. Typically left behind when a
  snapshot subvolume was removed by hand. `revenantctl cleanup` removes
  these alongside DELETE markers.
- **nested-subvolumes** — informational notice about nested subvolumes
  inside any snapshotted subvolume. Revenant re-attaches them across a
  restore at their current state, but their *contents* are not versioned
  along with the parent (see *Nested subvolumes* below for details).

```sh
sudo revenantctl check
```

## Nested subvolumes

Btrfs snapshots do not include subvolumes nested inside the snapshotted subvolume — they
appear as empty directories in the snapshot. This is a fundamental property of Btrfs, not
a revenant bug. It matters in practice because systemd automatically creates
`/var/lib/machines` and `/var/lib/portables` as nested subvolumes inside the root, and
users often create their own (e.g. `/var/lib/docker`, database stores, …).

### How revenant handles them

Snapshot and rollback treat nested subvolumes as **runtime state, not versioned content**:

- **Snapshots** capture only the parent subvolume's tree. The nested subvolumes are not
  copied. This is the unavoidable btrfs behaviour.
- **Restore** rolls back the parent subvolume and then re-attaches every nested subvolume
  it found beforehand to the same path inside the restored tree, in their *current* state.
  If the snapshot pre-dates the nested subvolume's parent path (e.g. you rolled back to a
  point before `/var/lib` existed), the missing path is materialised on the fly so the
  nested subvolume has somewhere to land.
- **DELETE markers** (`@-DELETE-...`) are emptied of nested subvolumes during restore, so
  retention/cleanup can remove them normally.
- **Crash recovery.** If a restore is interrupted between renaming `@` and re-attaching
  the nested subvolumes, the next state-changing command (`snapshot`, `restore`, `delete`,
  `cleanup`) will detect the orphaned nested subvolumes inside the DELETE marker and move
  them back into the live `@` automatically.

### What this means for you

Nested subvolume contents are **not versioned** — a rollback does not revert them. The
nested subvolume itself survives the restore (revenant re-attaches it), but anything
written inside it stays at its current state. There is no need to flatten your layout to
accommodate revenant; the only thing to keep in mind is that snapshots do not recurse into
nested subvolumes, so state you *want* rolled back must live inside the parent, not in a
nested one.

`revenantctl check` reports nested subvolumes inside configured snapshot sources as an
informational notice so you can make a conscious decision about where state lives.

## Configuration

```toml
[sys]
rootfs_subvol = "@"
snapshot_subvol = "@snapshots"
# Apply per-strain retention automatically after `revenantctl snapshot`.
# When false, snapshot creation never deletes anything; retention runs
# only when `cleanup` is invoked explicitly.
auto_apply_retention = true
# DELETE markers (`<base>-DELETE-<ts>` subvols left behind by a previous
# restore) auto-expire after this many days as part of any retention
# run. 0 disables auto-expiry; `cleanup --force` always purges them.
tombstone_max_age_days = 14

[sys.rootfs]
backend = "btrfs"
device_uuid = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

[sys.efi]
enabled = true
mount_point = "/boot"
staging_subvol = "@boot"

[sys.bootloader]
backend = "systemd-boot"

[strain.default]
subvolumes = ["@"]
efi = true

[strain.default.retain]
last = 5
daily = 7
```

The annotated, field-by-field reference is in
[`config/revenant.toml.example`](config/revenant.toml.example).

## Requirements

- Linux with a Btrfs root filesystem
- Optional: EFI system partition (for EFI sync; most valuable with systemd-boot)
- Rust 1.85+ (to build)

## Testing in a VM

Revenant performs btrfs subvolume operations and syncs an ESP — both are easier to
exercise without risk inside a throwaway VM. For an end-to-end test you need a guest
that mirrors the supported target shape: **UEFI firmware, systemd-boot, a Btrfs root
filesystem with a snapshottable subvolume layout** (e.g. `@` / `@home` / `@snapshots`).
Any installer that can produce that layout works; building such a VM by hand is
faster than auditing a third-party script that promises to do it for you. Once the
VM boots, `sudo revenantctl init` detects the layout and writes a starter config,
and the rest of the workflow is identical to the real-system commands above.

## Disclaimer

Revenant is provided **"as is", without warranty of any kind**, express or implied,
including but not limited to the warranties of merchantability, fitness for a particular
purpose, and non-infringement. In no event shall the author be liable for any claim,
damages, or other liability — including but not limited to **data loss, filesystem
corruption, unbootable systems, or any other direct, indirect, incidental, special,
exemplary, or consequential damages** — arising from, out of, or in connection with the
software or the use or other dealings in the software.

This software has been written to the best of the author's knowledge and ability, but
it may contain bugs. **Revenant is not a backup solution.** It takes point-in-time
snapshots of a live system and is designed to complement, not replace, a proper backup
strategy. Always use revenant alongside independent backup software that stores your
data on a separate device or medium. By using this software, you accept full
responsibility for any consequences of its operation on your system.

See the [LICENSE](LICENSE) file for the full legal terms.

## License

GPL-3.0-only — see [LICENSE](LICENSE).
