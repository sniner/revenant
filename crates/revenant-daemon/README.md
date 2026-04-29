# revenant-daemon

Privileged D-Bus daemon backing `revenant-gui` (and any other unprivileged
client that wants to talk to revenant). The CLI does **not** depend on
this daemon — see [the design doc][design] for the rationale.

[design]: ../../docs/design/dbus-interface.md

The daemon owns the btrfs toplevel mount for its entire runtime
(at `/run/revenant/toplevel`) and exposes the `dev.sniner.Revenant1`
interface on the **system bus**. Authorization for individual methods
is delegated to polkit. Per-call wire contract: see [the design doc][design].

The daemon is functionally complete for the Phase-1 GUI:

- **Metadata**: `GetVersion`, `GetDaemonInfo`.
- **Strain inspection**: `ListStrains`, `GetStrain`.
- **Snapshot read-path**: `ListSnapshots` (with optional strain
  filter), `GetSnapshot`, `GetLiveParent`.
- **Privileged writes** (all polkit-gated):
  `SetStrainRetention` (round-trip TOML edit),
  `CreateSnapshot` (synchronous),
  `DeleteSnapshot` (synchronous),
  `Restore` (synchronous, with optional `save_current` and
  preflight findings in the result).
- **Live updates** via inotify: `SnapshotsChanged`,
  `StrainConfigChanged`, `LiveParentChanged` (emitted from the
  restore path), debounced with a 200 ms trailing-edge window.

`DaemonStateChanged` is declared but not emitted yet. Hardening
items still open are listed under [What's not here yet](#whats-not-here-yet).

## Status

> [!WARNING]
> Alpha. Same caveat as the rest of the project — only run this in a
> throwaway VM. The daemon mounts the btrfs toplevel and holds it for
> its entire lifetime; a daemon crash leaves a stale mount under
> `/run/revenant/toplevel` (the next start self-heals it).

## Requirements

- A working revenant config at `/etc/revenant/config.toml`. Run
  `sudo revenantctl init` once if you have not yet.
- `dbus-broker` or `dbus-daemon` running on the system bus
  (every modern Linux distro).
- Root, because the daemon performs `mount(2)` and btrfs ioctls.

## Manual development install

Two policy files are required before the daemon is usable: a D-Bus
bus-policy (without it, startup fails with `AccessDenied` because the
daemon cannot claim its well-known name) and a polkit action policy
(without it, privileged methods fail with `Action … is not registered`
before they reach the daemon at all).

```sh
# From the repo root.

# 1. D-Bus bus access policy. Defines who may own the bus name and who
#    may call methods on it. The mkdir is needed on minimal installs
#    (e.g. Arch without a desktop environment) where the `dbus` package
#    only ships /usr/share/dbus-1/system.d/ and the admin override dir
#    is not created until something else needs it. Harmless on systems
#    where the dir already exists.
sudo mkdir -p /etc/dbus-1/system.d
sudo install -m644 data/dev.sniner.Revenant.conf /etc/dbus-1/system.d/

# 2. Polkit action definitions. Every privileged method
#    (SetStrainRetention / CreateSnapshot / DeleteSnapshot / Restore)
#    asks polkit for an action by id; without this file polkit replies
#    "action not registered" and the call fails before it ever reaches
#    the daemon's logic.
sudo install -m644 data/dev.sniner.Revenant.policy /usr/share/polkit-1/actions/

# 3. Reload the bus so it picks up the new policy. Use whichever unit
#    is running on your system — `dbus.service` on Arch and Debian-
#    based distros, `dbus-broker.service` on Fedora and recent SUSE.
#    Polkit needs no reload; it re-reads its actions/rules dirs on
#    every check.
sudo systemctl reload dbus.service   # or: dbus-broker.service
```

That's all that is required for development. The D-Bus
service-activation file (`data/dev.sniner.Revenant.service`) and the
systemd unit (`data/revenant-daemon.service`) are **not** needed while
you start the daemon by hand — they only matter for an installed
system where the daemon is meant to be started on demand by D-Bus
activation.

### Polkit auth on a desktop-less system

The privileged methods are gated by `auth_admin` actions, so polkit
tries to interactively authenticate the caller. On a normal desktop
session this is handled by an agent the DE starts (gnome-shell,
kwallet-polkit, …). On a minimal installation (SSH into a VM, no DE)
no agent is running and the call fails with `Access denied` — polkit
silently denies because it has no way to prompt.

Two options for development:

**Option (a): `pkttyagent` in the same shell.** Registers an agent for
the current shell's PID; polkit then prompts for the password in the
terminal:

```sh
pkttyagent --process $$ --notify-fd 3 3>&2 &
# now run busctl ... in the same shell
```

This relies on the SSH session being a logind session — check with
`loginctl show-session $XDG_SESSION_ID` if no prompt appears.

**Option (b): permissive polkit rule (dev VM only).** Bypass the auth
prompt entirely for `dev.sniner.Revenant.*` actions when the caller is in the
`wheel` group:

```sh
sudoedit /etc/polkit-1/rules.d/49-revenant-dev.rules
```

```javascript
// Dev-only: grant all dev.sniner.Revenant.* actions to wheel group without prompt.
// Remove this file before testing the real auth-prompt flow via the GUI.
polkit.addRule(function(action, subject) {
    if (action.id.indexOf("dev.sniner.Revenant.") === 0 &&
        subject.isInGroup("wheel")) {
        return polkit.Result.YES;
    }
});
```

No reload needed. **Remove the file** before testing the real
prompt flow (which is what the GUI will actually exercise).

## Running

```sh
sudo RUST_LOG=info cargo run --bin revenantd
```

Expected log lines on a healthy start:

```text
revenantd <version> starting
mounted btrfs toplevel (/dev/disk/by-uuid/…) on /run/revenant/toplevel
daemon ready
registered dev.sniner.Revenant on /dev/sniner/Revenant
snapshot watcher watching /run/revenant/toplevel/@snapshots
config watcher watching /etc/revenant
```

If the config is missing or the device cannot be mounted, the daemon
still starts but logs the reason and reports `degraded` via
`GetDaemonInfo` — that is by design, so the GUI can render a clean
error state instead of having no daemon at all to talk to.

## Smoke test

In a second shell:

```sh
# Daemon health.
busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 GetVersion

busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 GetDaemonInfo
```

`GetDaemonInfo` returns an `a{sv}` dict with at least:

- `version` — daemon version string
- `backend` — `"btrfs"`
- `toplevel_mounted` — `true` on a healthy daemon
- `toplevel_path` — `/run/revenant/toplevel`
- `device_uuid` — the configured rootfs device
- `degraded_reason` — present only when `toplevel_mounted = false`

```sh
# Strain inspection.
busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 ListStrains

busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 GetStrain s default

# Snapshot listing — empty filter means "all strains".
busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 ListSnapshots 'a{sv}' 0

# Same, but only the "default" strain.
busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 ListSnapshots 'a{sv}' 1 strain s default

# Live anchor — empty dict means "pristine system, no anchor".
busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 GetLiveParent
```

Cross-check against the CLI: `revenantctl list` should show the same
snapshots, with the `*`-marked anchor matching `GetLiveParent`'s
`(strain, id)`.

```sh
# Privileged writes — all of these will trigger a polkit prompt the
# first time, then re-use the cached auth (auth_admin_keep) for a
# few minutes. Restore (auth_admin) re-prompts every time.
#
# All snapshot lookups are strain-scoped: the (strain, id) pair must
# match what `ListSnapshots` reports. Passing the right id under the
# wrong strain returns "snapshot not found" — by design, so a typo
# in the strain name cannot accidentally hit a different namespace.

# Edit retention: set strain "default" to last=5, daily=14, others
# disabled. Note the array length (2) before the key/type/value
# triples.
busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 SetStrainRetention sa{sv} default 2 \
        last u 5 daily u 14

# Take a snapshot (empty message → omitted in the sidecar). The reply
# contains the new id; substitute it into the Delete call below.
busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 CreateSnapshot ss default ""

# Delete a snapshot by (strain, id). Replace 20260426-150156-977 with
# the id printed by CreateSnapshot above.
busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 DeleteSnapshot ss default 20260426-150156-977

# Restore: pick a (strain, id) pair from `ListSnapshots`. Dry-run
# first to inspect preflight findings without touching anything
# (same reply shape as a real restore, but `dry_run=true` and no
# state change).
busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 Restore ssa{sv} <strain> <id> 1 \
        dry_run b true

# Real restore with save_current (this is the default; shown
# explicitly here for completeness). Captures the current state as
# a pre-restore snapshot in the target strain before swapping over.
busctl --system call dev.sniner.Revenant /dev/sniner/Revenant \
    dev.sniner.Revenant1 Restore ssa{sv} <strain> <id> 1 \
        save_current b true
```

```sh
# Watch live updates. Then in a third shell, run something like
# `sudo revenantctl snapshot default -m test` and see a
# SnapshotsChanged signal appear here. After a Restore call, you
# also see LiveParentChanged.
busctl --system monitor dev.sniner.Revenant
```

Confirm the mount actually happened. `/run/revenant/` is created with
mode 0700 root, so these commands need sudo from a regular shell:

```sh
sudo findmnt /run/revenant/toplevel
```

Then stop the daemon with `Ctrl+C`. The mount-point should disappear:

```sh
sudo findmnt /run/revenant/toplevel    # → no output
sudo ls /run/revenant/                 # → empty (or directory gone)
```

## Cleaning up

If the daemon was killed hard (`SIGKILL`, panic, OOM) the mount can
survive across restarts. The next `revenantd` start detects this and
unmounts the stale mount before mounting fresh, so this is normally
self-healing.

To clean up by hand:

```sh
sudo umount /run/revenant/toplevel
sudo rmdir /run/revenant/toplevel /run/revenant
```

Removing the policy files:

```sh
sudo rm /etc/dbus-1/system.d/dev.sniner.Revenant.conf
sudo rm /usr/share/polkit-1/actions/dev.sniner.Revenant.policy
sudo rm -f /etc/polkit-1/rules.d/49-revenant-dev.rules   # if you used option (b)
sudo systemctl reload dbus.service                       # or: dbus-broker.service
```

## What's not here yet

- Per-strain granularity for `SnapshotsChanged` — currently the signal
  always fires with `strain=""` (= "any"). Clients refresh the whole
  list anyway in practice.
- `DaemonStateChanged` is declared but never emitted; intended for
  transitions in/out of degraded state.
- Custom `dev.sniner.Revenant.Error.*` D-Bus errors — currently everything
  not modelled by `fdo` goes through `org.freedesktop.DBus.Error.Failed`
  with a human-readable message. Functional, but not ideal for clients
  that want to branch on error kind.
- Idle-exit timer. The daemon stays up for the lifetime of the
  process; D-Bus activation will start it on demand once that path
  is wired, and an idle timeout would let it shut itself down again.
- D-Bus activation + a tested production install path. For now this
  is a `cargo run` daemon only; the systemd unit and bus-activation
  files in `data/` are stubs that have not been exercised end to end.
- Packaging (Make/just/xtask targets, distro packages). The bus
  policy must currently be installed by hand.
