//! Single-snapshot delete confirmation, plus the result handlers for
//! the related `Delete` and `SetSnapshotProtected` round-trips.
//!
//! The protect-toggle's apply lives here (rather than under its own
//! "protect" dialog) because there is no dialog — `snapshot_row`
//! optimistically flips the lock icon and dispatches the command
//! directly. The result handler only has to confirm or roll back.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use revenant_core::metadata::format_message_items;

use crate::dbus_thread::Command;
use crate::model::Snapshot;
use crate::ui::format::format_created;
use crate::ui::toast::show_progress_toast;
use crate::{AppState, Widgets};

/// Present the delete-snapshot confirmation. Single-snapshot delete:
/// strain + id are captured from the row's snapshot. Live-anchor rows
/// get an extra warning paragraph so the user knows what they're
/// trading away (the ★ reference, not the running system).
pub(crate) fn present_delete_dialog(
    parent: &adw::ApplicationWindow,
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    snap: &Snapshot,
) {
    let mut body = format!(
        "{} · {}\n\n\
         {}\n\n\
         The snapshot's subvolumes and metadata sidecar will be \
         removed. This cannot be undone.",
        snap.strain,
        format_created(snap),
        format_message_items(&snap.message).unwrap_or_else(|| "(no message)".to_string()),
    );
    if snap.is_live_anchor {
        body.push_str(
            "\n\nThis is the ★ live anchor — the snapshot the running \
             system descends from. Deleting it does not affect the \
             running system, but you will lose the reference point \
             that ties the live state to a named snapshot.",
        );
    }

    let dialog = adw::AlertDialog::builder()
        .heading("Delete snapshot?")
        .body(body)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let snap_strain = snap.strain.clone();
    let snap_id = snap.id.clone();
    let widgets = widgets.clone();
    let state = Rc::clone(state);
    let cmd_tx = cmd_tx.clone();
    dialog.connect_response(None, move |_dlg, response| {
        if response != "delete" {
            return;
        }

        state.borrow_mut().delete_in_flight = true;

        let toast = show_progress_toast(
            &widgets.toast_overlay,
            "Deleting snapshot — waiting for authentication…",
            "Deleting snapshot…",
        );
        state.borrow_mut().delete_progress_toast = Some(toast);

        let _ = cmd_tx.send_blocking(Command::DeleteSnapshot {
            strain: snap_strain.clone(),
            id: snap_id.clone(),
        });
    });

    dialog.present(Some(parent));
}

pub(crate) fn apply_delete_snapshot_result(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    strain: &str,
    id: &str,
    result: Result<(), String>,
) {
    {
        let mut st = state.borrow_mut();
        st.delete_in_flight = false;
        if let Some(toast) = st.delete_progress_toast.take() {
            toast.dismiss();
        }
    }

    match result {
        Ok(()) => {
            widgets
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("Deleted {strain}@{id}")));
            // List refresh comes via SnapshotsChanged.
        }
        Err(reason) => {
            tracing::warn!("DeleteSnapshot({strain}@{id}) failed: {reason}");
            widgets
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("Delete failed: {reason}")));
        }
    }
}

/// Reconcile a `SetSnapshotProtected` round-trip with the optimistic
/// icon flip in `snapshot_row`.
///
/// On success the daemon's inotify watcher fires `SnapshotsChanged`,
/// which already triggers a list reload — nothing further is needed
/// here. On failure we surface the daemon's reason as a toast and
/// kick a manual reload of the current strain so the row rebuilds
/// with the (unchanged) on-disk state, undoing the optimistic flip.
pub(crate) fn apply_set_snapshot_protected_result(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    strain: &str,
    id: &str,
    requested: bool,
    result: Result<Snapshot, String>,
) {
    match result {
        Ok(_snap) => {
            let action = if requested {
                "Protected"
            } else {
                "Unprotected"
            };
            widgets
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("{action} {strain}@{id}")));
        }
        Err(reason) => {
            tracing::warn!("SetSnapshotProtected({strain}@{id}={requested}) failed: {reason}");
            widgets
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("Protect change failed: {reason}")));
            // Reload the visible strain so the row rebuilds from the
            // on-disk truth, reverting the optimistic icon flip.
            let selected = state.borrow().selected_strain.clone();
            if let Some(sel) = selected
                && sel == strain
            {
                let _ = cmd_tx.send_blocking(Command::LoadSnapshots(sel));
            }
        }
    }
}
