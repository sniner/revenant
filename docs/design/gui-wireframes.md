# GUI Wireframes — `revenant-gui`

Status: **draft** — design document, not yet implemented.

Phase 1 scope: list snapshots (with lineage tree), edit retention per
strain, trigger restore. Architecture must accommodate later additions
(create snapshot, delete, edit message, manage strains) without a layout
rewrite.

## Stack

- GTK 4 + libadwaita (`gtk4-rs`, `libadwaita-rs`).
- Architecture layer: `relm4` — keeps the view code declarative and the
  model/update loop testable.
- D-Bus client: `zbus` against `revenant-daemon` (see
  [dbus-interface.md](dbus-interface.md)).

## Conventions

The GUI follows the **Gnome HIG**:

- Adwaita widgets only (no plain GTK headerbars, no GtkPaned where Adw
  splits exist, etc.).
- `AdwApplicationWindow` + `AdwHeaderBar`.
- Sidebar layout via `AdwOverlaySplitView` (collapses on narrow widths
  → mobile-ready out of the box).
- Settings/edit dialogs use `AdwPreferencesDialog` with `AdwPreferencesPage`/
  `AdwPreferencesGroup`.
- Destructive confirmations use `AdwAlertDialog` with the destructive
  button styled `destructive-action`.
- Empty/loading/error states use `AdwStatusPage`.
- Toast notifications via `AdwToastOverlay` for non-modal feedback
  ("Snapshot created", "Restore scheduled — reboot required").
- Standard keybindings: `Ctrl+Q` quit, `Ctrl+R` refresh, `F10` menu, etc.
- Light/dark follows system, no custom theming.
- Window default size 1100×720, minimum 360×500 (mobile).

## App layout

Three-pane layout via nested `AdwOverlaySplitView`:

1. Strain sidebar (left, collapsible on narrow widths).
2. Snapshot list for the selected strain (centre).
3. Detail pane for the selected snapshot (right, collapsible).

```
┌────────────────────────────────────────────────────────────────────────────┐
│ ☰  Revenant                                                    [⟳]  [⋮]    │  ← AdwHeaderBar
├────────────┬───────────────────────────────────────┬───────────────────────┤
│ Strains    │  default          [Edit retention…]   │  manual               │
│            │  ────────────────────────────────────  │  2026-04-13 14:22     │
│ ● default  │                                       │                       │
│   boot     │  2026-04-15 06:00  periodic           │  "kernel update"      │
│   periodic │  2026-04-14 09:11  manual             │                       │
│   pacman   │   "before tinkering"                  │  Trigger    manual    │
│            │  2026-04-13 14:22  manual    ★ anchor │  Strain     default   │
│ ────────── │   "kernel update"                     │  Created    14:22:08  │
│ Live state │  2026-04-13 06:00  periodic           │  Subvols    @, @home  │
│ ★ default  │  2026-04-12 19:00  periodic           │  Size       412 MiB   │
│   on 2026- │  2026-04-12 18:03  pre-restore        │                       │
│   04-13    │   "auto: pre-restore save"            │  [Protected]          │
│   14:22:08 │  2026-04-11 22:14  manual             │                       │
│            │   "before BIOS update"                │  ┌─────────────────┐  │
│            │  …                                    │  │  Restore…       │  │
│            │                                       │  └─────────────────┘  │
│            │                                       │  [Delete]  (Phase 2)  │
└────────────┴───────────────────────────────────────┴───────────────────────┘
```

Sidebar: `AdwNavigationSplitView` with the strain list. A pinned
"Live state" footer block shows what the running system descends from
(strain + id + created-timestamp), based on `GetLiveParent()`. The same
`★` glyph is used in the sidebar footer and as the per-row anchor
marker, so the eye can match them across panes. If `GetLiveParent()`
returns empty (pristine system / anchor lost), the footer reads
"Pristine — no restore yet."

Content: a flat snapshot list (`GtkListView` over a sorted model,
newest-first), grouped only by visual day-separators if useful. One row
carries strain badge, timestamp, optional message, and — for the single
anchor snapshot — a `★ anchor` pill. Selecting a row updates the detail
pane and (if the sidebar is collapsed) navigates to it on narrow widths.
The `★` marker mirrors the CLI `*` from `revenantctl list`.

### Detail pane

A pane on the right side of the central split, always visible on wide
windows, slidable on narrow ones. Renders the selected snapshot:

- Big timestamp + strain.
- Message (italic; "—" if missing).
- Key/value rows: trigger, subvolumes, created (full timestamp + UTC
  offset), size (from `Snapshot.size_bytes`; "—" if `0`).
- Pills: `Protected` if retention claims it; `★ Anchor` if it's the live
  parent.
- Primary action: **Restore…** (suggested-action style).
- Secondary actions: **Delete** (greyed in Phase 1), context menu with
  "Copy id", "Show in file manager" (jumps to the sidecar dir).

When nothing is selected: `AdwStatusPage` placeholder ("Select a snapshot
to see details").

### Snapshot row

```
  2026-04-13 14:22  manual                                    ★ anchor
   "kernel update"                                            [protected]
```

- Two-line list row (`GtkListView` with a custom factory).
- Right-click / long-press: Restore, Copy id, Delete (greyed if
  protected or in Phase 1).
- The `★ anchor` pill is shown on at most one row per strain. When the
  current strain has no anchor (live system descends from a different
  strain or is pristine), no row carries it.

### Header bar

- Hamburger menu (`☰`): About, Preferences (app-level, not strain),
  Quit.
- Refresh button (`⟳`): manual reload; in practice rarely needed because
  of inotify-driven `LineageChanged`/`SnapshotsChanged` signals.
- Kebab (`⋮`): "Take snapshot…" (Phase 2), "Open config file…" (jumps to
  `/etc/revenant/config.toml` — for now the safety net for anything the
  GUI can't do).

## Edit retention dialog

`AdwPreferencesDialog`, single page, single group. Each tier as
`AdwSpinRow` (0 = disabled). Save commits via
`SetStrainRetention()` — toast on success, inline error banner on
failure.

```
┌─ Retention — default ────────────────────────────────────────┐
│                                                              │
│  Snapshots are kept according to tiered policies. A snapshot │
│  is retained as long as any tier still claims it. Set a tier │
│  to 0 to disable it.                                         │
│                                                              │
│  Last                                              [  3  ▾]  │
│   Most recent snapshots, regardless of age.                  │
│                                                              │
│  Hourly                                            [ 24  ▾]  │
│   Newest per clock-hour for N hours.                         │
│                                                              │
│  Daily                                             [  7  ▾]  │
│   Newest per calendar-day for N days.                        │
│                                                              │
│  Weekly                                            [  4  ▾]  │
│   Newest per ISO-week for N weeks.                           │
│                                                              │
│  Monthly                                           [  6  ▾]  │
│   Newest per calendar-month for N months.                    │
│                                                              │
│  Yearly                                            [  2  ▾]  │
│   Newest per calendar-year for N years.                      │
│                                                              │
│  ⚠ With Last = 0 and only longer tiers active, a same-day    │
│    pre-restore snapshot can evict an older same-day pick.    │
│                                                              │
│                                       [ Cancel ]  [ Save ]   │
└──────────────────────────────────────────────────────────────┘
```

The footgun warning at the bottom is contextual — appears only when
`last == 0` and any of `daily/weekly/monthly/yearly > 0`. Matches the
known `--save-current` retention footgun (kept by design; see project
memory).

Note: for now we do not edit `subvolumes` or `efi` from the GUI. Those
remain config-file decisions; the dialog ends with a small "Other
strain settings are managed in `/etc/revenant/config.toml`." link.

## Restore confirmation

Restore is the most consequential action. Two-step flow:

### Step 1 — `AdwAlertDialog`, "Restore this snapshot?"

```
┌─ Restore snapshot? ──────────────────────────────────────────┐
│                                                              │
│  default · 2026-04-13 14:22                                  │
│  "kernel update"                                             │
│                                                              │
│  This will replace the current system state. The running     │
│  system will be rolled back at the next reboot.              │
│                                                              │
│  ☑ Save the current state as a snapshot first                │
│      (recommended — lets you undo this restore)              │
│                                                              │
│  ☐ Dry run (plan only, do not execute)                       │
│                                                              │
│                                  [ Cancel ]  [ Restore ▼ ]   │
│                                  ─────────                   │
│                                  destructive-action style    │
└──────────────────────────────────────────────────────────────┘
```

- "Save current state" is checked by default. Maps to the `save_current`
  option of the `Restore()` D-Bus call.
- Polkit prompt is triggered by the D-Bus call itself; the user sees the
  system polkit dialog directly after clicking *Restore*.

### Step 2 — Toast + status banner

```
  ┌─ Restore complete ───────────────────────────────────────────┐
  │ The system will boot from the restored snapshot at the next  │
  │ reboot. A pre-restore snapshot was saved as default-…         │
  │                                              [ Reboot now ▸] │
  └──────────────────────────────────────────────────────────────┘
```

`AdwBanner` at the top of the window with a "Reboot now" action. The
toast is shorter and ephemeral; the banner persists until dismissed or
until the user reboots.

## Empty / error states

Each via `AdwStatusPage`:

- **No daemon connection** — "Cannot reach `revenant-daemon`. Is the
  service running?" with a "Try again" button. (zbus reconnects in the
  background; this state means initial connection failed.)
- **No snapshots** — "No snapshots in this strain yet." with a
  "Take a snapshot" button (greyed in Phase 1).
- **Toplevel not mounted** — daemon reports
  `BackendUnavailable`. "Snapshot storage is not available — check the
  configuration." with a link to the config file.

## Live updates

`zbus::Proxy` subscriptions to the daemon signals drive the model:

- `SnapshotsChanged(strain)` → invalidate the shown strain's list.
- `LiveParentChanged` → re-fetch live parent and update the anchor
  pill + sidebar footer.
- `StrainConfigChanged` → re-fetch strain list and refresh the retention
  dialog if open.

The model layer keeps a small reconciler so repeated identical updates
don't trigger full rebuilds (selection preservation matters when a
periodic snapshot lands and the list re-sorts).

## Out of scope (Phase 1)

- Taking snapshots from the GUI (button is shown but disabled).
- Deleting snapshots from the GUI.
- Editing snapshot messages.
- Adding/removing strains.
- EFI status/diagnostics views.
- A "diff between snapshots" view (interesting, but big — separate
  feature).
- Undo of a restore in the GUI (the user reboots into the saved-current
  snapshot manually for now).

## Open questions

1. **Where does "live state" live in the sidebar.** Pinned footer (as
   sketched) vs. always-visible top item, vs. a dedicated "Status" page
   alongside the strain pages. Footer feels right for now — small,
   peripheral, always visible — but worth revisiting once the app has
   actually been used.
2. **Day-separators in the snapshot list.** Useful for periodic-heavy
   strains where many rows fall on the same day. Skip in v1, add if it
   feels cramped.
