//! Restore-snapshot dialog: two checkboxes (`save_current`, `dry_run`),
//! dispatches a `Command::Restore`, then renders the result as a toast
//! plus the reboot banner on success.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use revenant_core::metadata::format_message_items;

use crate::dbus_thread::Command;
use crate::model::Snapshot;
use crate::ui::format::format_created;
use crate::ui::toast::show_progress_toast;
use crate::{AppState, Widgets};

/// Build and present the AdwAlertDialog for a Restore action. The
/// dialog has two checkboxes (`save_current`, `dry_run`) following
/// the wireframe; on confirmation it dispatches a `Command::Restore`
/// and hands the in-flight bookkeeping to the result handler.
pub(crate) fn present_restore_dialog(
    parent: &adw::ApplicationWindow,
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    snap: &Snapshot,
) {
    let heading = "Restore snapshot?";
    let body = format!(
        "{} · {}\n\n\
         {}\n\n\
         This will replace the current system state. The running \
         system will be rolled back at the next reboot.",
        snap.strain,
        format_created(snap),
        format_message_items(&snap.message).unwrap_or_else(|| "(no message)".to_string()),
    );

    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("restore", "Restore");
    dialog.set_response_appearance("restore", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    // Extra child: two checkboxes laid out vertically with the
    // copy from the wireframe. `save_current` defaults to true
    // (recommended); `dry_run` defaults to false.
    let save_check = gtk::CheckButton::builder()
        .label("Save the current state as a snapshot first")
        .active(true)
        .build();
    let save_hint = gtk::Label::builder()
        .label("Recommended — lets you undo this restore.")
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .margin_start(28)
        .build();
    let dry_check = gtk::CheckButton::builder()
        .label("Dry run (plan only, do not execute)")
        .active(false)
        .build();
    let extra = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(8)
        .build();
    extra.append(&save_check);
    extra.append(&save_hint);
    extra.append(&dry_check);
    dialog.set_extra_child(Some(&extra));

    let snap_strain = snap.strain.clone();
    let snap_id = snap.id.clone();
    let widgets = widgets.clone();
    let state = Rc::clone(state);
    let cmd_tx = cmd_tx.clone();
    dialog.connect_response(None, move |_dlg, response| {
        if response != "restore" {
            return;
        }
        let save_current = save_check.is_active();
        let dry_run = dry_check.is_active();

        // Mark in-flight before sending so the result handler can
        // assume the start state when it clears the flag. Per-row
        // Restore buttons read this flag and no-op while it's set.
        {
            let mut st = state.borrow_mut();
            st.restore_in_flight = true;
        }

        let toast = if dry_run {
            // Dry-run is fast and not really restoring anything —
            // pass the same label twice so the OperationStarted
            // swap is a visual no-op.
            show_progress_toast(
                &widgets.toast_overlay,
                "Running preflight checks…",
                "Running preflight checks…",
            )
        } else {
            show_progress_toast(
                &widgets.toast_overlay,
                "Restoring snapshot — waiting for authentication…",
                "Restoring snapshot…",
            )
        };
        state.borrow_mut().restore_progress_toast = Some(toast);

        let _ = cmd_tx.send_blocking(Command::Restore {
            strain: snap_strain.clone(),
            id: snap_id.clone(),
            save_current,
            dry_run,
        });
    });

    dialog.present(Some(parent));
}

pub(crate) fn apply_restore_result(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    req_strain: &str,
    req_id: &str,
    result: Result<crate::model::RestoreOutcome, String>,
) {
    {
        let mut st = state.borrow_mut();
        st.restore_in_flight = false;
        if let Some(toast) = st.restore_progress_toast.take() {
            toast.dismiss();
        }
    }

    match result {
        Ok(outcome) if outcome.dry_run => {
            // Dry-run: no live state changed. We surface the outcome
            // as a single toast — full preflight-findings rendering
            // is deferred (the daemon already includes them in the
            // result dict, but a proper findings dialog is its own
            // little design).
            let toast = adw::Toast::new("Dry run complete — preflight passed. No changes applied.");
            widgets.toast_overlay.add_toast(toast);
        }
        Ok(outcome) => {
            let extra = match outcome.pre_restore {
                Some((strain, id)) => format!(" · pre-restore: {strain}@{id}"),
                None => String::new(),
            };
            let title = format!(
                "Restore complete — {strain}@{id}{extra}",
                strain = outcome.restored_strain,
                id = outcome.restored_id,
            );
            widgets.toast_overlay.add_toast(adw::Toast::new(&title));
            widgets.reboot_banner.set_revealed(true);
        }
        Err(reason) => {
            tracing::warn!("Restore({req_strain}, {req_id}) failed: {reason}");
            let toast = adw::Toast::new(&format!("Restore failed: {reason}"));
            widgets.toast_overlay.add_toast(toast);
        }
    }
}
