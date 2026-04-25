# D-Bus Interface — `revenant-daemon`

Status: **draft** — design document, not yet implemented.

## Overview

`revenantd` is a system-bus D-Bus service that owns the privileged operations
of revenant: it mounts the btrfs toplevel, watches it via inotify, and
exposes snapshot/strain/restore operations through a versioned D-Bus API.
GUI clients (and, optionally, future TUIs or higher-level tools) talk to it
unprivileged. Authorization is delegated to polkit per action.

The CLI (`revenantctl`) deliberately does **not** use the daemon — it keeps
its standalone, daemon-free operation. The daemon and the CLI share
`revenant-core`; logic lives there, both binaries are thin frontends.

## Naming

| Item             | Value                                |
| ---------------- | ------------------------------------ |
| Bus              | system bus                           |
| Service name     | `org.revenant.Daemon1`               |
| Object path      | `/org/revenant/Daemon`               |
| Primary iface    | `org.revenant.Daemon1`               |
| Activation       | systemd-activated (`dbus-broker`)    |
| Implementation   | `zbus` (Rust)                        |

The trailing `1` in the names is the API version. Breaking changes ship as
`org.revenant.Daemon2` alongside the old one for a deprecation period.

## Lifecycle

- D-Bus-activated. The daemon starts when the first method call comes in
  and exits after an idle timeout (default 5 min) if no clients hold a
  watch.
- On startup it mounts the btrfs toplevel (UUID from
  `/etc/revenant/config.toml`) on `/run/revenant/toplevel` (private mount,
  `MountFlags=private` in the unit so the host namespace is unaffected).
- `inotify` watches the snapshot directory and the config directory.
- On exit (clean or signal) the toplevel is unmounted.

## Polkit actions

| Action                          | Default rule        | Notes                                  |
| ------------------------------- | ------------------- | -------------------------------------- |
| `org.revenant.list`             | `yes`               | Read-only listing, allowed for any user. |
| `org.revenant.snapshot.create`  | `auth_admin_keep`   | Cached for the standard polkit window. |
| `org.revenant.snapshot.delete`  | `auth_admin_keep`   | Cached.                                |
| `org.revenant.config.edit`      | `auth_admin_keep`   | Cached.                                |
| `org.revenant.restore`          | `auth_admin`        | **Not** cached — restore is the riskiest action and should always re-prompt. |

The polkit policy file ships in `data/org.revenant.policy` and is installed
to `/usr/share/polkit-1/actions/`.

## Types

D-Bus types are spelled in dbus signature notation. Rust struct names are
shown for clarity; serialization uses `zbus`/`zvariant` `SerializeDict`/
`Type` derives where it pays off (extensible structs as `a{sv}`), and
plain tuples otherwise.

### `Strain` — `(sasba{sv})`

```text
name:        s    -- "default", "boot", ...
subvolumes:  as   -- ["@", "@home"]
efi:         b
retention:   a{sv}  -- {"last": u32, "hourly": u32, "daily": u32, ...}
```

`retention` is an `a{sv}` (extensible dict) so future tiers don't break the
wire format. Known keys: `last`, `hourly`, `daily`, `weekly`, `monthly`,
`yearly`, all `u`. Missing keys mean "0 / disabled".

### `Snapshot` — `a{sv}`

Returned as an extensible dict to allow growth without a version bump.
Defined keys (initial set):

| Key                | Type | Description                                              |
| ------------------ | ---- | -------------------------------------------------------- |
| `id`               | `s`  | Snapshot id (`YYYYMMDD-HHMMSS` UTC).                     |
| `strain`           | `s`  | Strain name.                                             |
| `created`          | `s`  | RFC 3339 timestamp.                                      |
| `trigger`          | `s`  | `manual` \| `pacman` \| `systemd-boot` \| `systemd-periodic` \| `restore` \| `unknown`. |
| `message`          | `s`  | User-supplied note (may be empty).                       |
| `is_live_anchor`   | `b`  | True if this snapshot is the parent of the live rootfs (mirror of CLI `*` marker; matches `revenant_core::snapshot::resolve_live_parent`). |
| `is_protected`     | `b`  | True if retention currently protects this snapshot.      |
| `size_bytes`       | `t`  | Best-effort size; `0` if unknown.                        |

### `LiveParent` — `a{sv}`

Mirrors `revenant_core::snapshot::LiveParentRef`. Empty dict if the
live rootfs has no resolvable parent (pristine system, or anchor lost).

```text
{
  "strain": s,    -- e.g. "default"
  "id":     s,    -- e.g. "20260413-142200"
}
```

## Methods

All methods are on the primary interface `org.revenant.Daemon1` unless
otherwise noted.

### Discovery / metadata

```text
GetVersion() -> (s)                             -- e.g. "0.2.0"
GetDaemonInfo() -> (a{sv})                      -- {"version", "backend", "toplevel_mounted": b, ...}
```

### Strains

```text
ListStrains() -> (a(sasba{sv}))                 -- array of Strain
GetStrain(name: s) -> (sasba{sv})
SetStrainRetention(name: s, retention: a{sv})   -- privileged: org.revenant.config.edit
```

`SetStrainRetention` rewrites only the `[strain.<name>.retain]` section in
`/etc/revenant/config.toml`, preserving comments and unrelated keys
(round-trip TOML edit).

For Phase 1 only retention is editable from the GUI. Adding/removing
strains, changing `subvolumes`, etc. is intentionally out of scope and
remains a config-file edit.

### Snapshots

```text
ListSnapshots(filter: a{sv}) -> (aa{sv})        -- filter: optional {"strain": s}
GetSnapshot(strain: s, id: s) -> (a{sv})
CreateSnapshot(strain: s, message: s) -> (a{sv}) -- privileged: org.revenant.snapshot.create
                                                 --   returns the new Snapshot dict
DeleteSnapshot(strain: s, id: s) -> ()          -- privileged: org.revenant.snapshot.delete
```

`CreateSnapshot` and `DeleteSnapshot` complete well under a second
on btrfs, so they return synchronously rather than going through the
`OperationHandle` pattern. Pass the empty string as `message` to omit
it. Both operations cause the inotify watcher to fire
`SnapshotsChanged`, which is how unprivileged subscribers learn about
the change — the methods do not emit anything themselves.

### Live state

```text
GetLiveParent() -> (a{sv})                      -- LiveParent dict; empty if none
```

The GUI uses this to mark the corresponding row in the snapshot list and
to fill the sidebar's "running on top of …" footer. There is intentionally
no full lineage / ancestry tree on the wire — the CLI does not expose one
either, and the surface stays small.

### Restore

```text
Restore(strain: s, id: s, options: a{sv}) -> (a{sv}) -- privileged: org.revenant.restore
```

`options` keys (initial set):
- `save_current` (`b`) — take a `--save-current` snapshot first
  (default: `true`).
- `dry_run` (`b`) — run preflight checks only and return without
  touching subvolumes (default: `false`).

Returns a result dict with:
- `restored_id`, `restored_strain` — echo of the input.
- `pre_restore_id`, `pre_restore_strain` — present iff a save-current
  snapshot was created.
- `dry_run` (`b`) — `true` iff `options.dry_run` was set; in that
  case no other keys describe state changes.
- `findings` (`aa{sv}`) — preflight findings (severity / check / message /
  optional hint). Always present, may be empty. `Severity::Error` items
  block the restore unless overridden.

A successful restore emits `LiveParentChanged` so subscribed clients
know to re-fetch `GetLiveParent`.

Restore is intentionally **synchronous** rather than going through an
`OperationHandle`. The full pipeline — pre-restore snapshot, EFI sync,
subvolume rename, live-state refresh — completes well under 10 s on
typical hardware, and partial-cancel is dangerous once the rename is
in progress. If a future profile shows EFI sync turning into a real
multi-minute operation, an additional `RestoreAsync` returning a handle
can be added without breaking this signature.

## Signals

On `org.revenant.Daemon1`:

```text
SnapshotsChanged(strain: s)                     -- "" means: any/all
StrainConfigChanged()
LiveParentChanged()                             -- emitted after a successful restore
DaemonStateChanged(state: s, message: s)        -- "ready", "degraded", "error"
```

Clients are expected to subscribe via `Match` rules (zbus does this
automatically with property/signal proxies).

## Errors

D-Bus errors use the `org.revenant.Error.*` namespace, each mapping back
to a variant of `revenant_core::RevenantError`:

| D-Bus error                              | Maps to                            |
| ---------------------------------------- | ---------------------------------- |
| `org.revenant.Error.NotAuthorized`       | polkit denial                      |
| `org.revenant.Error.NotFound`            | unknown strain or snapshot id      |
| `org.revenant.Error.InvalidArgument`     | malformed input                    |
| `org.revenant.Error.Conflict`            | concurrent operation in progress   |
| `org.revenant.Error.BackendUnavailable`  | toplevel not mounted, btrfs missing |
| `org.revenant.Error.Internal`            | catch-all; details in message      |

## Concurrency

- The daemon serializes write-ops (snapshot create / delete / restore /
  config edit) on a single tokio mutex. Reads run unsynchronized off a
  cached snapshot index that the inotify watcher invalidates.
- Only one restore may be in flight at a time. A second `Restore()` while
  one is running fails with `Conflict`.

## Open questions

1. **Live progress for Restore.** Restore is essentially atomic (subvol
   rename), so `Progress` signals are mostly cosmetic (0% → 100%). Pre-
   restore actions (save-current snapshot, EFI sync) are the slow parts
   and *can* report fine-grained progress. Design the operation interface
   for that case from the start.
2. **EFI strain coupling.** `efi=true` strains touch the ESP. Should that
   be a separate polkit action? Probably no; restore covers it.
3. **Mount lifecycle on idle exit.** If the daemon idles out and the
   toplevel was idle-mounted by it, it should umount on exit. If the user
   had it mounted manually for some reason — out of scope, daemon only
   manages its own private mount under `/run/revenant/`.

## Out of scope (Phase 1)

- User-bus variant of the daemon (would require user-namespaced btrfs
  mounts; not feasible for the rootfs).
- Adding/removing/renaming strains via D-Bus.
- Sidecar editing (changing `message`, etc.) via D-Bus.
- Subscription pacing / batching (we'll see if signal storms become an
  issue first).
