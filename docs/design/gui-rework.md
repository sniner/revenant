# GUI Rework — Visual Polish Pass

Status: **draft v2** — wireframe for review, not yet implemented.

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

**Out of scope:** changing the KV-block. It works correctly and stays
in place (description / trigger rows under the timestamp). Only the
big text-pill buttons next to it shrink to icons.

## Current state vs. target

```
CURRENT                                            TARGET
┌──────────┬──────────────────────────────┐        ┌──────────┬──────────────────────────────┐
│          │                              │        │ Strains  │ default                      │
│ default  │  29.04.2026 11:00:14         │        │──────────│──────────────────────────────│
│ boot     │  Description: revenant-…     │        │ default  │ ┌──────────────────────────┐ │
│ pacman   │  Trigger:     systemd-…      │        │ 6 ∙ 29.4.│ │ 29. April 2026, 11:00:14 │ │
│ periodic │ ┌─────────┐ ┌─────────┐      │        │ ──────── │ │ Description: revenant-…  │ │
│ ●●●●●●●● │ │ Restore │ │ Delete  │      │  ───►  │ boot     │ │ Trigger:     systemd-…   │ │
│          │ └─────────┘ └─────────┘      │        │ 3 ∙ 29.4.│ │                  [↻] [✕] │ │
│          │                              │        │ ──────── │ ├──────────────────────────┤ │
│          │ 29.04.2026 10:09:02          │        │ ★ peri.  │ │ 29. April 2026, 10:09:02 │ │
│          │ ┌─────────┐ ┌─────────┐      │        │ 6 ∙ 29.4.│ │                  [↻] [✕] │ │
│          │ │ Restore │ │ Delete  │      │        │ ──────── │ ├──────────────────────────┤ │
│          │ └─────────┘ └─────────┘      │        │ pacman   │ │ 28. April 2026, 19:00:05 │ │
│          │                              │        │ 12 ∙ 29.4.│ │                 [↻] [✕] │ │
│          │                              │        │          │ └──────────────────────────┘ │
│ no title │                              │        │          │                              │
│ 220-320  │                              │        │ 180-260  │                              │
└──────────┴──────────────────────────────┘        └──────────┴──────────────────────────────┘
```

## Sidebar (strains pane)

**Title at the top.** Today the sidebar starts cold with the strain
list — no header. Add a short heading "Strains" as a small label with
the `["heading"]` (or `["title-3"]`) CSS class above the ListBox, so
users see what the column is. Same treatment on the right pane: a
heading row showing the currently-selected strain name (large, with
the existing retention/add-snapshot buttons next to it).

**Width.** Drop `min_sidebar_width` from 220 to 180 and
`max_sidebar_width` from 320 to 260. The sidebar is a navigation list,
not a workspace; today's 220-pixel-wide names are wasted real estate.
**No drag handle** — the auto-width has to land it without the user
having to grab a divider every time. (Persisted state for a
draggable handle would be its own can of worms.)

**Per-strain content (`AdwActionRow`).** Title + single-line subtitle,
suffix is the existing ★ live-anchor marker:

| Element  | Style                                        | Content example         |
| -------- | -------------------------------------------- | ----------------------- |
| Title    | bold, default size                           | `default`               |
| Subtitle | dim/`["caption", "dim-label"]`, regular weight | `6 snapshots · 29.4.`   |
| Suffix   | accent-pill, only on the live-anchor strain  | `★`                     |

If a strain has no snapshots, subtitle reads "no snapshots yet".

**Auto-update.** This is the part that needs a wiring change. Today
`SignalSnapshotsChanged(strain)` only triggers a reload of the
**currently selected** strain's snapshot list. The sidebar stats
would go stale until the user re-selects.

Fix without a daemon API change: on every `SnapshotsChanged` event
(specific or empty), the GUI fires one extra `ListSnapshots(filter={})`
in parallel and groups the result client-side, then re-renders the
sidebar subtitles. One call per event, refreshes all strains at once.
The user-visible cost is minor; on every snapshot create/delete one
extra small RPC. If profiling shows it as a hotspot we can later add
`snapshot_count` and `latest_id` to `ListStrains`'s wire tuple — but
that's a separate slice and a wire-format change.

## Snapshot row

The KV-block stays. Two changes:

### Buttons → icon buttons

```
CURRENT                                       TARGET (delete-icon variant a)
┌─────────┐  ┌─────────┐                                    ┌──┐  ┌──┐
│ Restore │  │ Delete  │                                    │↻ │  │🗑 │
└─────────┘  └─────────┘                                    └──┘  └──┘
   pill        pill+destructive             flat+circular   flat+circular+destructive

                                              TARGET (delete-icon variant b)
                                                            ┌──┐  ┌──┐
                                                            │↻ │  │✕ │
                                                            └──┘  └──┘
```

- Restore: `gtk::Button` with `icon_name("view-refresh-symbolic")`,
  `tooltip_text("Restore snapshot")`, `css_classes(["flat", "circular"])`.
- Delete: `tooltip_text("Delete snapshot")`,
  `css_classes(["flat", "circular", "destructive-action"])`. Two
  candidate icons:
  - **Variant (a)** `user-trash-symbolic` — Adwaita's standard "delete
    this item" glyph. Semantically correct for "permanent removal".
  - **Variant (b)** `window-close-symbolic` — the familiar `×`. Common
    in compact lists; reads more as "dismiss" than "delete", but the
    destructive-action red ring carries the danger.

Both are valid; defer the pick to the implementation step (cheap to
flip).

The `flat` + `circular` pair is the same recipe used by the existing
header buttons (`main.rs:259-267`, retention/cleanup) — the visuals
already fit. Tooltip carries the action name for discoverability.

### Layout

The headline row, the KV-block (Description / Trigger), and the
button group all stay at their current positions. Buttons move from
under the KV-block up to the right edge of the headline row, since
they shrink from pill-buttons to icons and don't deserve a row of
their own anymore.

```
┌──────────────────────────────────────────────────┐
│ 29. April 2026, 11:00:14            [↻]    [✕]   │
│ Description:  revenant-periodic.service          │
│ Trigger:      systemd-periodic                   │
└──────────────────────────────────────────────────┘
```

For snapshots without sidecar metadata the KV-block is naturally
empty and the row is just the headline plus buttons. That's correct
behaviour, not a layout flaw.

## Style anchor: retention editor

What works in the retention editor:

- `AdwPreferencesGroup` for the rounded-card container.
- `AdwSpinRow` rows with comfortable spacing.
- A single contextual hint near the bottom, styled as a `caption`.

What we adopt for the main view:

- **Snapshot list as a card**, not bare rows. Wrap the `ListBox` in
  an `AdwPreferencesGroup` (or a `frame` with matching CSS) so it
  gets the same rounded-corner panel treatment as the retention
  card. Today the rows float on the toolbar background, which is
  what makes the right pane feel unfinished.
- **Generous row padding** — the retention editor's rows breathe;
  the snapshot rows don't. Match that spacing.
- **Day separators** as low-emphasis subheads between groups of
  rows on the same calendar day. Small caption-class label, not a
  full-width line. Optional — try the card wrapping first and add
  separators only if the list still looks like a wall of text.

## What stays the same

- Two-pane layout (`OverlaySplitView`).
- Header bar: title, refresh, cleanup notification, kebab.
- KV-block content (description / trigger).
- The retention editor (the anchor, no changes).
- The cleanup / pre-restore review dialog (already lives in a
  PreferencesGroup card).
- The restore confirmation dialog (already correct: AlertDialog with
  the two relevant checkboxes; the inline "Last=0 footgun" warning
  there can be removed in a follow-up — Slice 1b made the warning
  obsolete).

## Implementation order (Slice 5b)

After approval of this wireframe:

1. Sidebar + content-pane title labels.
2. Strains-pane width constraints + subtitle enrichment, plus the
   client-side fan-out reload on `SnapshotsChanged`.
3. Snapshot-row buttons → icon buttons; relocate to the headline row.
4. Snapshot-list card wrapper + matching row padding.
5. Optional: day separators if step 4 still looks dense.
6. Optional follow-up (not strictly part of this rework): drop the
   now-stale Last=0 warning from the retention dialog.

Each step independently shippable.

## Out of scope for this rework

- A custom application icon — separate slice (currently the
  packaging falls back to `document-revert`).
