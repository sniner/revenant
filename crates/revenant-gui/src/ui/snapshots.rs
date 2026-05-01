//! Snapshot list pane — populates the centre column with one row per
//! snapshot for the currently selected strain. Each row carries its
//! own Restore / Delete / lock-toggle buttons that capture the snapshot
//! directly so the click handler refers to the correct entry.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use revenant_core::metadata::format_message_items;

use crate::dbus_thread::Command;
use crate::model::Snapshot;
use crate::ui::dialogs::delete::present_delete_dialog;
use crate::ui::dialogs::restore::present_restore_dialog;
use crate::ui::format::{format_created, kv_pair};
use crate::{AppState, Widgets};

pub(crate) fn apply_snapshots(
    parent: &adw::ApplicationWindow,
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    strain: &str,
    result: Result<Vec<Snapshot>, String>,
) {
    // Race protection: if the user has switched strain since this
    // request was sent, drop the result on the floor.
    if state.borrow().selected_strain.as_deref() != Some(strain) {
        return;
    }

    // Preserve scroll position across in-place refreshes (signal-
    // driven reloads). On the very first load the adjustment value
    // is 0 anyway, so this is a no-op for new selections.
    let scroll_value = widgets.snapshot_scroll.vadjustment().value();

    while let Some(child) = widgets.snapshot_list.first_child() {
        widgets.snapshot_list.remove(&child);
    }

    match result {
        Ok(snaps) if snaps.is_empty() => {
            state.borrow_mut().snapshots.clear();
            widgets.snapshot_empty.set_description(Some(
                "This strain has no snapshots yet. Use the + button above to create one.",
            ));
            widgets.snapshot_stack.set_visible_child_name("empty");
        }
        Ok(snaps) => {
            // Daemon sorts oldest-first by id; reverse for newest-on-top
            // display matching the wireframes.
            let ordered: Vec<Snapshot> = snaps.into_iter().rev().collect();
            for snap in &ordered {
                widgets
                    .snapshot_list
                    .append(&snapshot_row(snap, parent, widgets, state, cmd_tx));
            }

            state.borrow_mut().snapshots = ordered;
            widgets.snapshot_stack.set_visible_child_name("list");

            // Re-apply the scroll position once the list has had a
            // chance to allocate. idle_add_local_once runs on the
            // next main-loop tick, after the ListBox children have
            // been measured.
            let scroll = widgets.snapshot_scroll.clone();
            glib::idle_add_local_once(move || {
                scroll.vadjustment().set_value(scroll_value);
            });
        }
        Err(reason) => {
            state.borrow_mut().snapshots.clear();
            tracing::warn!("ListSnapshots({strain}) failed: {reason}");
            widgets.snapshot_error.set_description(Some(&reason));
            widgets.snapshot_stack.set_visible_child_name("error");
        }
    }
}

/// Build a snapshot row: bold date headline with action buttons on
/// the right, then an aligned key/value block (Description, Trigger).
/// Empty values are skipped so the row stays compact.
///
/// Both action buttons capture the snapshot directly so each row's
/// click handler refers to the right snapshot. The Delete dialog
/// warns extra-strongly when the row is the live anchor — the daemon
/// allows it (matching CLI semantics), but the user loses the ★
/// reference and that's worth flagging.
fn snapshot_row(
    snap: &Snapshot,
    parent: &adw::ApplicationWindow,
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
) -> gtk::ListBoxRow {
    // Headline: ★ marker (live anchor) + date/time + spacer + buttons.
    // CSS class "heading" gives bold body-size text — the user pushed
    // back on title-3 (too large vs the sidebar strain titles).
    let anchor_marker = gtk::Label::builder()
        .label(if snap.is_live_anchor { "★" } else { " " })
        .css_classes(if snap.is_live_anchor {
            vec!["accent", "heading"]
        } else {
            vec!["heading"]
        })
        .width_chars(2)
        .build();

    let date = gtk::Label::builder()
        .label(format_created(snap))
        .xalign(0.0)
        .css_classes(["heading"])
        .build();

    // Lock toggle: closed = protected, open = unprotected. The icon
    // flips optimistically on click (snappy feel under polkit latency);
    // the daemon-side `SnapshotsChanged` reload reconciles or the
    // failure handler reverts.
    let lock_btn = gtk::Button::builder()
        .icon_name(if snap.protected {
            "changes-prevent-symbolic"
        } else {
            "changes-allow-symbolic"
        })
        .tooltip_text(if snap.protected {
            "Protected — click to allow retention and deletion"
        } else {
            "Unprotected — click to protect from retention and deletion"
        })
        .css_classes(["flat", "circular"])
        .build();
    let restore_btn = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Restore snapshot")
        .css_classes(["flat", "circular"])
        .build();
    let delete_btn = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Delete snapshot")
        .css_classes(["flat", "circular", "destructive-action"])
        .build();
    if snap.protected {
        delete_btn.set_sensitive(false);
        delete_btn.set_tooltip_text(Some(
            "Snapshot is protected — click the lock first to allow deletion.",
        ));
    } else if snap.is_live_anchor {
        delete_btn.set_tooltip_text(Some(
            "Delete snapshot. This snapshot is the parent of the running \
             system; deleting it removes the ★ live-anchor reference, the \
             running system itself is unaffected.",
        ));
    }

    let headline = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    headline.append(&anchor_marker);
    headline.append(&date);
    let spacer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .hexpand(true)
        .build();
    headline.append(&spacer);
    headline.append(&lock_btn);
    headline.append(&restore_btn);
    headline.append(&delete_btn);

    // K/V block. Fixed column width keeps values aligned across rows.
    // Order is invariant; absent values omit the row instead of
    // showing an empty value. Description = the daemon's pre-formatted
    // summary (trigger detail + message) — same content as the CLI
    // `list` Description column minus the leading trigger kind.
    let kv = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_start(28) // line up with the date (after the ★ slot)
        .build();
    if let Some(desc) = format_message_items(&snap.message) {
        kv.append(&kv_pair("Description:", &desc));
    }
    if !snap.trigger.is_empty() && snap.trigger != "unknown" {
        kv.append(&kv_pair("Trigger:", &snap.trigger));
    }

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(16)
        .margin_end(16)
        .build();
    body.append(&headline);
    if kv.first_child().is_some() {
        body.append(&kv);
    }

    let row = gtk::ListBoxRow::builder().child(&body).build();

    {
        let snap = snap.clone();
        let cmd_tx = cmd_tx.clone();
        let lock_btn = lock_btn.clone();
        lock_btn.connect_clicked(move |btn| {
            // Flip the icon and tooltip immediately for a snappy feel
            // under polkit latency. The daemon's `SnapshotsChanged`
            // reload (success path) replaces this row anyway; on
            // failure the result handler kicks a list reload that
            // reverts.
            let new_protected = !snap.protected;
            btn.set_icon_name(if new_protected {
                "changes-prevent-symbolic"
            } else {
                "changes-allow-symbolic"
            });
            btn.set_tooltip_text(Some(if new_protected {
                "Protected — click to allow retention and deletion"
            } else {
                "Unprotected — click to protect from retention and deletion"
            }));
            // Avoid double-fires while the round-trip is in flight.
            btn.set_sensitive(false);
            let _ = cmd_tx.send_blocking(Command::SetSnapshotProtected {
                strain: snap.strain.clone(),
                id: snap.id.clone(),
                protected: new_protected,
            });
        });
    }

    {
        let snap = snap.clone();
        let widgets = widgets.clone();
        let state = Rc::clone(state);
        let cmd_tx = cmd_tx.clone();
        let parent = parent.clone();
        restore_btn.connect_clicked(move |_| {
            if state.borrow().restore_in_flight {
                return;
            }
            present_restore_dialog(&parent, &widgets, &state, &cmd_tx, &snap);
        });
    }

    {
        let snap = snap.clone();
        let widgets = widgets.clone();
        let state = Rc::clone(state);
        let cmd_tx = cmd_tx.clone();
        let parent = parent.clone();
        delete_btn.connect_clicked(move |_| {
            if state.borrow().delete_in_flight {
                return;
            }
            present_delete_dialog(&parent, &widgets, &state, &cmd_tx, &snap);
        });
    }

    row
}
