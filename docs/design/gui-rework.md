# GUI Rework — Visual Polish Pass

Status: **draft** — wireframe for review, not yet implemented.

## Why

User feedback after `feat/gui` landed (notebook test, 2026-04-29):

> Das Pane mit den Strains ist a) extrem langweilig b) zu breit und in
> der Größe fixiert.
>
> Die Restore/Delete Knöpfe in jedem Snapshot sind riesig.
>
> Das UI ist optisch nicht ausgewogen, wirkt anfängerhaft. Der einzige
> Dialog mit Charme ist der Retention-Editor.

This pass reworks the **visuals and proportions** of the existing
two-pane layout. No new screens, no new functionality. The retention
editor stays as the stylistic anchor — its card-with-rows look
generalises to the rest of the app.

**Out of scope:** the KV-block in snapshot rows. It works correctly
(populated when sidecar metadata exists, empty otherwise); old
sidecars without metadata are the only reason rows ever look bare,
and that population is by design.

## Current state vs. target

```
CURRENT                                              TARGET
┌────────────┬────────────────────────────────┐     ┌────────┬────────────────────────────────┐
│            │                                │     │        │                                │
│ Strains    │  Snapshot rows                 │     │ Strains│  Snapshot rows                 │
│            │                                │     │        │                                │
│ default    │  29.04.2026 11:00:14           │     │ default│  29.04.2026 11:00:14           │
│ boot       │   Description: revenant-…      │     │  6 ↻   │  ↻ revenant-periodic.service   │
│ pacman     │   Trigger:     systemd-…       │     │  29 Apr│  📅 29.04.2026             [↻] [🗑]│
│ periodic   │  ┌────────┐  ┌────────┐        │     │ ────── │  ────────────────────────       │
│ ●●●●●●●●   │  │Restore │  │ Delete │        │     │ boot   │  29.04.2026 10:09:02       [↻] [🗑]│
│            │  └────────┘  └────────┘        │     │  3 ↻   │  ────────────────────────       │
│            │                                │     │  28 Apr│  28.04.2026 19:00:05       [↻] [🗑]│
│            │  29.04.2026 10:09:02           │     │ pacman │                                │
│            │  ┌────────┐  ┌────────┐        │     │  12 ↻  │                                │
│            │  │Restore │  │ Delete │        │     │  29 Apr│                                │
│            │  └────────┘  └────────┘        │     │ ────── │                                │
│            │                                │     │ ★ peri │                                │
│            │                                │     │  6 ↻   │                                │
│ 220-320 px │                                │     │ 180-260│                                │
│ fixed      │                                │     │ user-  │                                │
│            │                                │     │ resiz. │                                │
└────────────┴────────────────────────────────┘     └────────┴────────────────────────────────┘
```

## Strains pane

**Width.** Drop `min_sidebar_width` from 220 to 180 and
`max_sidebar_width` from 320 to 260. The sidebar is a navigation list,
not a workspace; 220-pixel-wide strain names are wasted real estate.

**Sidebar resize.** `AdwOverlaySplitView` does not natively expose a
draggable divider. Two options:

- **A (recommended)**: keep `OverlaySplitView` with the new
  min/max constraints. Adwaita's automatic narrow-mode collapse and
  swipe-to-reveal still work. No drag handle.
- **B**: switch to `gtk::Paned`. Gives the user a draggable divider
  but loses the responsive collapse behaviour and the swipe gesture.

User feedback was "in der Größe fixiert", which can be read either as
"too wide for what's there" (A solves it) or "I want to drag it" (B).
A is the lighter touch — recommend trying that first; if it still feels
fixed, B is the fallback.

**Content per strain.** Each `AdwActionRow` gets a richer subtitle:

```
default
6 snapshots · 29 Apr
```

- Title: strain display name, falling back to the technical id
  (already implemented).
- Subtitle line 1 ("6 snapshots"): count from
  `Strain.snapshot_count`, computed from the snapshots-changed
  notification.
- Subtitle line 2 ("29 Apr"): localised short-form date of the
  newest snapshot in this strain.
- Suffix: ★ live-anchor marker (already implemented).

If a strain has zero snapshots, subtitle reads "no snapshots yet"
(single line; takes the row's natural shorter height).

Adwaita's `ActionRow` only shows one subtitle string by default.
Either join with a middle dot — `6 snapshots · 29 Apr` — or use
two-line via `subtitle-lines`. Single-line with separator is more
compact and matches the rest of the app.

## Snapshot row

Two changes: button treatment and a tighter header line.

### Buttons → icon buttons

```
CURRENT                                          TARGET
                                                         ↻ Restore (tooltip)
┌─────────┐  ┌─────────┐                         ┌──┐  ┌──┐
│ Restore │  │ Delete  │                         │↻ │  │🗑 │
└─────────┘  └─────────┘                         └──┘  └──┘
   pill        pill+destructive                 flat   flat+destructive
```

- Restore: `gtk::Button::builder().icon_name("view-refresh-symbolic")
  .tooltip_text("Restore snapshot")
  .css_classes(["flat", "circular"])`
- Delete: `icon_name("user-trash-symbolic")
  .tooltip_text("Delete snapshot")
  .css_classes(["flat", "circular", "destructive-action"])`

The `flat` + `circular` pair is the same recipe used by the existing
header buttons (`main.rs:259-267`, retention/cleanup) — the visuals
already fit. Tooltip carries the action name for discoverability.

The destructive flavour stays on Delete via the standard CSS class —
the icon hover ring tints red, matching the retention-editor's Delete
language.

### Row header

Drop the explicit "Description:" / "Trigger:" KV labels for snapshots
where the metadata is short (the common case — one description line,
one trigger). Inline them with small icons or a dot separator:

```
HEADLINE  29. April 2026, 11:00:14                     [↻]  [🗑]
META       ↻ revenant-periodic.service · systemd-periodic
```

- `↻` glyph (or trigger-specific icon: `🔧` manual, `📦` pacman,
  `🕐` periodic, `↻` restore) tags the trigger and replaces the
  "Trigger:" label.
- Description text follows directly, separated by middle dot.
- Empty meta block → row collapses to header line only (one row,
  no awkward labelled-but-empty KV).

For multi-line messages (pacman with several packages) keep the
existing layout but drop the explicit "Description:" label — the
icon prefix carries the meaning.

## Style anchor: retention editor

What works in the retention editor:

- `AdwPreferencesGroup` for the rounded-card container.
- `AdwSpinRow` (= `AdwActionRow` with a numeric suffix) for clean
  rows.
- Comfortable vertical spacing inside the card; the dialog isn't
  cramped.
- A single contextual hint near the bottom (the footgun warning),
  styled as a `caption` not a banner.

What we adopt for the main view:

- **Snapshot list as a card**, not bare rows. Wrap the `ListBox` in
  an `AdwPreferencesGroup` (or a `frame` + matching CSS) so it has
  the same rounded-corner panel treatment as the retention card.
  Currently the rows float on the toolbar background, which is what
  makes the right pane feel unfinished.
- **Day separators** as low-emphasis subheads between groups of
  rows on the same calendar day. Small caption-class label, not a
  full-width line. Keeps the "newest-first scroll" intuitive without
  requiring the user to compare timestamps row by row.
- **Generous row padding** — the retention editor's rows breathe;
  the snapshot rows don't. Match the spacing.

## What stays the same

- Two-pane layout (`OverlaySplitView`).
- Header bar: title, refresh, cleanup notification, kebab.
- The retention editor (the anchor, no changes).
- The cleanup / pre-restore review dialog (already lives in a
  PreferencesGroup card).
- The restore confirmation dialog (already correct: AlertDialog with
  the two relevant checkboxes).

## Implementation order (Slice 5b)

After approval of this wireframe:

1. Strains-pane width + subtitle enrichment (`apply_strains`,
   `OverlaySplitView` constraints).
2. Snapshot-row buttons → icon buttons (`snapshot_row` body).
3. Snapshot-list card wrapper + day separators.
4. Row-header inlining (drop labelled KV, use icon-prefix meta).

Each step independently shippable; (1) and (2) are risk-free CSS
swaps and worth landing first to confirm the toolchain works on the
branch.

## Out of scope for this rework

- A real `gtk::Paned` resize handle for the sidebar (decision
  deferred until A is on the user's screen).
- A custom application icon — separate slice (currently the
  packaging falls back to `document-revert`).
- Tree / lineage view — explicitly rejected in the project memory;
  the ★ live-anchor marker is the whole "ancestry" surface.
