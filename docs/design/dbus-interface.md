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
| Service name     | `dev.sniner.Revenant`                |
| Object path      | `/dev/sniner/Revenant`               |
| Primary iface    | `dev.sniner.Revenant1`               |
| Activation       | systemd-activated (`dbus-broker`)    |
| Implementation   | `zbus` (Rust)                        |

The trailing `1` on the **interface** is the API version. The service name and
the object path stay unversioned — the daemon's identity does not change when
the wire contract evolves. Breaking changes add a sibling interface
(`dev.sniner.Revenant2`) on the same object path; clients negotiate which one
they speak. This follows the modern freedesktop convention (NetworkManager,
UPower) rather than the systemd1/UDisks2 style of versioning the service name.

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

| Action                                | Default rule        | Notes                                  |
| ------------------------------------- | ------------------- | -------------------------------------- |
| `dev.sniner.Revenant.list`            | `yes`               | Read-only listing, allowed for any user. |
| `dev.sniner.Revenant.snapshot.create` | `auth_admin_keep`   | Cached for the standard polkit window. |
| `dev.sniner.Revenant.snapshot.delete` | `auth_admin_keep`   | Cached.                                |
| `dev.sniner.Revenant.config.edit`     | `auth_admin_keep`   | Cached.                                |
| `dev.sniner.Revenant.restore`         | `auth_admin`        | **Not** cached — restore is the riskiest action and should always re-prompt. |
| `dev.sniner.Revenant.cleanup`         | `auth_admin_keep`   | Purges pre-restore DELETE markers.     |

The polkit policy file ships in `data/dev.sniner.Revenant.policy` and is
installed to `/usr/share/polkit-1/actions/`.

## Types

D-Bus types are spelled in dbus signature notation. Rust struct names are
shown for clarity; serialization uses `zbus`/`zvariant` `SerializeDict`/
`Type` derives where it pays off (extensible structs as `a{sv}`), and
plain tuples otherwise.

### `Strain` — `(sasba{sv}s)`

```text
name:          s     -- "default", "boot", ...
subvolumes:    as    -- ["@", "@home"]
efi:           b
retention:     a{sv} -- {"last": u32, "hourly": u32, "daily": u32, ...}
display_name:  s     -- friendly label or "" when not configured
```

`retention` is an `a{sv}` (extensible dict) so future tiers don't break the
wire format. Known keys: `last`, `hourly`, `daily`, `weekly`, `monthly`,
`yearly`, all `u`. Missing keys mean "0 / disabled".

`display_name` mirrors `[strain.<name>] display_name = "..."` from the
config. The empty string means "no display name configured"; clients
should fall back to `name`.

### `Snapshot` — `a{sv}`

Returned as an extensible dict to allow growth without a version bump.
Defined keys (initial set):

| Key                | Type | Description                                              |
| ------------------ | ---- | -------------------------------------------------------- |
| `id`               | `s`  | Snapshot id (`YYYYMMDD-HHMMSS` UTC).                     |
| `strain`           | `s`  | Strain name.                                             |
| `created`          | `s`  | RFC 3339 timestamp.                                      |
| `trigger`          | `s`  | `manual` \| `pacman` \| `systemd-boot` \| `systemd-periodic` \| `restore` \| `unknown`. |
| `message`          | `as` | Trigger-dependent list. `manual`: user note(s). `pacman`: package names. `systemd-boot`/`-periodic`: unit name. `restore`: source snapshot reference (`strain@id`). Omitted entirely if empty. |
| `is_live_anchor`   | `b`  | True if this snapshot is the parent of the live rootfs (mirror of CLI `*` marker; matches `revenant_core::snapshot::resolve_live_parent`). |
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

All methods are on the primary interface `dev.sniner.Revenant1` unless
otherwise noted.

### Discovery / metadata

```text
GetVersion() -> (s)                             -- e.g. "0.2.0"
GetDaemonInfo() -> (a{sv})                      -- {"version", "backend", "toplevel_mounted": b, ...}
```

### Strains

```text
ListStrains() -> (a(sasba{sv}s))                -- array of Strain
GetStrain(name: s) -> (sasba{sv}s)
GetLatestStrain() -> (s)                        -- name of strain whose newest snapshot
                                                --   has the most recent timestamp; "" if
                                                --   no strain has any snapshot. Lets the
                                                --   GUI pick a sensible initial selection.
SetStrainRetention(name: s, retention: a{sv})   -- privileged: dev.sniner.Revenant.config.edit
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
CreateSnapshot(strain: s, message: as) -> (a{sv}) -- privileged: dev.sniner.Revenant.snapshot.create
                                                  --   returns the new Snapshot dict
DeleteSnapshot(strain: s, id: s) -> ()           -- privileged: dev.sniner.Revenant.snapshot.delete
```

`CreateSnapshot` and `DeleteSnapshot` complete well under a second
on btrfs, so they return synchronously rather than going through the
`OperationHandle` pattern. Pass an empty array as `message` to omit it
(snapshots taken via the GUI are always tagged `manual`). Both
operations cause the inotify watcher to fire `SnapshotsChanged`, which
is how unprivileged subscribers learn about the change — the methods do
not emit anything themselves.

### Pre-restore states (DELETE markers)

A `restore` renames the previous live subvolume(s) to `<base>-DELETE-<ts>`
so the user has a safety net to roll back to. They are *not* snapshots
and do not appear in `ListSnapshots`. They survive until an explicit
cleanup — `revenantctl cleanup` from the CLI side, or the GUI via these
methods.

```text
ListDeleteMarkers() -> (aa{sv})                 -- array of DeleteMarker
PurgeDeleteMarkers(names: as) -> (as)           -- privileged: dev.sniner.Revenant.cleanup
                                                --   returns names actually removed
```

`DeleteMarker` keys (`a{sv}`):

| Key            | Type | Description                                              |
| -------------- | ---- | -------------------------------------------------------- |
| `name`         | `s`  | Full subvolume name, e.g. `"@-DELETE-20260411-080055"`.  |
| `base_subvol`  | `s`  | The live subvol this was renamed from (`"@"`, `"@home"`).|
| `id`           | `s`  | Snapshot id encoded in the marker's timestamp suffix.    |
| `created`      | `s`  | RFC 3339 timestamp parsed from `id`. Omitted if unparseable. |

Names that no longer match a live marker are silently skipped from the
returned set — a concurrent CLI cleanup may have purged them between
the listing and the user's confirmation.

A successful purge emits `DeleteMarkersChanged`. A successful `Restore`
also emits it (the restore creates a new marker).

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
Restore(strain: s, id: s, options: a{sv}) -> (a{sv}) -- privileged: dev.sniner.Revenant.restore
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

On `dev.sniner.Revenant1`:

```text
SnapshotsChanged(strain: s)                     -- "" means: any/all
StrainConfigChanged()
LiveParentChanged()                             -- emitted after a successful restore
DaemonStateChanged(state: s, message: s)        -- "ready", "degraded", "error"
DeleteMarkersChanged()                          -- after a successful PurgeDeleteMarkers
                                                --   or Restore
```

Clients are expected to subscribe via `Match` rules (zbus does this
automatically with property/signal proxies).

## Errors

D-Bus errors use the `dev.sniner.Revenant.Error.*` namespace, each mapping
back to a variant of `revenant_core::RevenantError`:

| D-Bus error                                       | Maps to                            |
| ------------------------------------------------- | ---------------------------------- |
| `dev.sniner.Revenant.Error.NotAuthorized`         | polkit denial                      |
| `dev.sniner.Revenant.Error.NotFound`              | unknown strain or snapshot id      |
| `dev.sniner.Revenant.Error.InvalidArgument`       | malformed input                    |
| `dev.sniner.Revenant.Error.PreflightBlocked`      | restore preflight reported `Severity::Error` findings |
| `dev.sniner.Revenant.Error.Conflict`              | concurrent operation in progress   |
| `dev.sniner.Revenant.Error.BackendUnavailable`    | toplevel not mounted, btrfs missing |
| `dev.sniner.Revenant.Error.Internal`              | catch-all; details in message      |

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
