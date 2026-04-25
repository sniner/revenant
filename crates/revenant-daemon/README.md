# revenant-daemon

Privileged D-Bus daemon backing `revenant-gui` (and any other unprivileged
client that wants to talk to revenant). The CLI does **not** depend on
this daemon — see [the design doc][design] for the rationale.

[design]: ../../docs/design/dbus-interface.md

The daemon owns the btrfs toplevel mount for its entire runtime
(at `/run/revenant/toplevel`) and exposes the `org.revenant.Daemon1`
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

The daemon needs a D-Bus bus-policy file installed before it is allowed
to claim the well-known name `org.revenant.Daemon1` on the system bus.
Without it, startup fails with `Permission denied: org.freedesktop.DBus.Error.AccessDenied`.

```sh
# From the repo root.

# 1. D-Bus bus access policy. Defines who may own the bus name and who
#    may call methods on it. Required.
sudo install -m644 data/org.revenant.Daemon1.conf /etc/dbus-1/system.d/

# 2. Reload the bus so it picks up the new policy.
sudo systemctl reload dbus-broker.service   # or: dbus.service
```

That's all that is required for development. The polkit policy
(`data/org.revenant.policy`), the D-Bus service-activation file
(`data/org.revenant.Daemon1.service`), and the systemd unit
(`data/revenant-daemon.service`) are **not** needed while you start
the daemon by hand — they only matter for an installed system where
the daemon is meant to be started on demand by D-Bus activation.

## Running

```sh
sudo RUST_LOG=info cargo run --bin revenantd
```

Expected log lines on a healthy start:

```text
revenantd 0.1.5 starting
mounted btrfs toplevel (/dev/disk/by-uuid/…) on /run/revenant/toplevel
daemon ready
registered org.revenant.Daemon1 on /org/revenant/Daemon
```

If the config is missing or the device cannot be mounted, the daemon
still starts but logs the reason and reports `degraded` via
`GetDaemonInfo` — that is by design, so the GUI can render a clean
error state instead of having no daemon at all to talk to.

## Smoke test

In a second shell:

```sh
# Daemon health.
busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 GetVersion

busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 GetDaemonInfo
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
busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 ListStrains

busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 GetStrain s default

# Snapshot listing — empty filter means "all strains".
busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 ListSnapshots 'a{sv}' 0

# Same, but only the "default" strain.
busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 ListSnapshots 'a{sv}' 1 strain s default

# Live anchor — empty dict means "pristine system, no anchor".
busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 GetLiveParent
```

Cross-check against the CLI: `revenantctl list` should show the same
snapshots, with the `*`-marked anchor matching `GetLiveParent`'s
`(strain, id)`.

```sh
# Privileged writes — all of these will trigger a polkit prompt the
# first time, then re-use the cached auth (auth_admin_keep) for a
# few minutes. Restore (auth_admin) re-prompts every time.

# Edit retention: set strain "default" to last=5, daily=14, others
# disabled. Note the array length (2) before the key/type/value
# triples.
busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 SetStrainRetention sa{sv} default 2 \
        last u 5 daily u 14

# Take a snapshot (empty message → omitted in the sidecar).
busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 CreateSnapshot ss default ""

# Delete a snapshot by (strain, id).
busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 DeleteSnapshot ss default 20260413-142200

# Restore: dry-run first to see preflight findings without touching
# anything (returns the same shape as a real restore, but with
# dry_run=true and no state change).
busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 Restore ssa{sv} default 20260413-142200 1 \
        dry_run b true

# Real restore with save_current (this is the default; shown
# explicitly here for completeness).
busctl --system call org.revenant.Daemon1 /org/revenant/Daemon \
    org.revenant.Daemon1 Restore ssa{sv} default 20260413-142200 1 \
        save_current b true
```

```sh
# Watch live updates. Then in a third shell, run something like
# `sudo revenantctl snapshot --strain manual -m test` and see a
# SnapshotsChanged signal appear here. After a Restore call, you
# also see LiveParentChanged.
busctl --system monitor org.revenant.Daemon1
```

Confirm the mount actually happened:

```sh
findmnt /run/revenant/toplevel
```

Then stop the daemon with `Ctrl+C`. The mount-point should disappear:

```sh
findmnt /run/revenant/toplevel    # → no output
ls /run/revenant/                 # → empty (or directory gone)
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

Removing the bus policy:

```sh
sudo rm /etc/dbus-1/system.d/org.revenant.Daemon1.conf
sudo systemctl reload dbus-broker.service
```

## What's not here yet

- Per-strain granularity for `SnapshotsChanged` — currently the signal
  always fires with `strain=""` (= "any"). Clients refresh the whole
  list anyway in practice.
- `DaemonStateChanged` is declared but never emitted; intended for
  transitions in/out of degraded state.
- Custom `org.revenant.Error.*` D-Bus errors — currently everything
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
