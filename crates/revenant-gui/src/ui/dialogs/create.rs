//! Create-snapshot dialog: optional message entry, dispatches a
//! `Command::CreateSnapshot`, renders the result toast.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::dbus_thread::Command;
use crate::model::Snapshot;
use crate::ui::toast::show_progress_toast;
use crate::{AppState, Widgets};

/// Present the create-snapshot dialog. AdwAlertDialog with one
/// AdwEntryRow for the optional message; strain is implicit (from
/// the header, passed in as `strain`). Trigger is implicitly
/// `manual` — the daemon hardcodes that for D-Bus CreateSnapshot.
pub(crate) fn present_create_snapshot_dialog(
    parent: &adw::ApplicationWindow,
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    strain: &str,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Create snapshot in {strain}?"))
        .body(
            "A new snapshot of this strain's subvolumes will be \
             created and recorded as a manual snapshot. You can \
             optionally tag it with a short message.",
        )
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("create", "Create");
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");

    // AdwEntryRow inside an AdwPreferencesGroup matches the
    // rounded-card style of the retention dialog.
    let message_row = adw::EntryRow::builder().title("Message (optional)").build();
    let group = adw::PreferencesGroup::builder().build();
    group.add(&message_row);
    dialog.set_extra_child(Some(&group));

    let strain_for_cb = strain.to_string();
    let widgets_for_cb = widgets.clone();
    let state_for_cb = Rc::clone(state);
    let cmd_tx_for_cb = cmd_tx.clone();
    dialog.connect_response(None, move |_dlg, response| {
        if response != "create" {
            return;
        }
        let text = message_row.text().to_string();
        let message = if text.is_empty() {
            Vec::new()
        } else {
            vec![text]
        };

        // Mark in-flight + disable the `+` button so a second click
        // during the polkit prompt can't queue a duplicate request.
        state_for_cb.borrow_mut().create_in_flight = true;
        widgets_for_cb.strain_btn_create.set_sensitive(false);

        let toast = show_progress_toast(
            &widgets_for_cb.toast_overlay,
            "Creating snapshot — waiting for authentication…",
            "Creating snapshot…",
        );
        state_for_cb.borrow_mut().create_progress_toast = Some(toast);

        let _ = cmd_tx_for_cb.send_blocking(Command::CreateSnapshot {
            strain: strain_for_cb.clone(),
            message,
        });
    });

    dialog.present(Some(parent));
}

pub(crate) fn apply_create_snapshot_result(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    strain: &str,
    result: Result<Snapshot, String>,
) {
    {
        let mut st = state.borrow_mut();
        st.create_in_flight = false;
        if let Some(t) = st.create_progress_toast.take() {
            t.dismiss();
        }
    }
    widgets.strain_btn_create.set_sensitive(true);

    match result {
        Ok(snap) => {
            widgets.toast_overlay.add_toast(adw::Toast::new(&format!(
                "Snapshot created: {}@{}",
                snap.strain, snap.id
            )));
            // The list itself refreshes through 4f's
            // SnapshotsChanged subscription; nothing to do here.
        }
        Err(reason) => {
            tracing::warn!("CreateSnapshot({strain}) failed: {reason}");
            widgets
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("Create failed: {reason}")));
        }
    }
}
