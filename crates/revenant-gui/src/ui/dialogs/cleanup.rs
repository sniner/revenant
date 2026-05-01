//! Pre-restore-states (tombstone) machinery: button refresh on
//! `ListDeleteMarkers`, the review-and-purge dialog, and the result
//! handler. Three pieces but one mental concept.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::dbus_thread::Command;
use crate::model::Tombstone;
use crate::ui::format::show_error_toast;
use crate::ui::toast::show_progress_toast;
use crate::{AppState, Widgets};

/// Refresh the header-bar cleanup button after a `ListDeleteMarkers`
/// reply (initial fetch or `DeleteMarkersChanged` follow-up). Hidden
/// when the list is empty so the header stays clean in the common
/// case; otherwise the button shows the count and a tooltip naming
/// the concept in plain language.
pub(crate) fn apply_tombstones(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    result: Result<Vec<Tombstone>, String>,
) {
    let tombstones = match result {
        Ok(t) => t,
        Err(reason) => {
            tracing::warn!("ListDeleteMarkers failed: {reason}");
            show_error_toast(
                &widgets.toast_overlay,
                "Could not load pre-restore states",
                &reason,
            );
            Vec::new()
        }
    };
    let count = tombstones.len();
    state.borrow_mut().tombstones = tombstones;

    let btn = &widgets.header_btn_cleanup;
    if count == 0 {
        btn.set_visible(false);
    } else {
        btn.set_visible(true);
        // ButtonContent is a private child; update its label by
        // walking the child once. Simpler than caching a separate
        // handle on Widgets — this runs at most a few times per
        // session.
        if let Some(content) = btn
            .child()
            .and_then(|c| c.downcast::<adw::ButtonContent>().ok())
        {
            content.set_label(&count.to_string());
        }
        let tooltip = if count == 1 {
            "1 pre-restore state ready to review".to_string()
        } else {
            format!("{count} pre-restore states ready to review")
        };
        btn.set_tooltip_text(Some(&tooltip));
    }
}

/// Build and present the pre-restore-states review dialog. One row
/// per tombstone, each with its own checkbox; default is "all
/// checked" so the typical case (user opens, hits Purge) is one
/// click. Confirm dispatches `Command::PurgeTombstones`; the result
/// arrives via `Event::PurgeTombstonesResult`.
pub(crate) fn present_cleanup_dialog(
    parent: &adw::ApplicationWindow,
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
) {
    let tombstones = state.borrow().tombstones.clone();
    if tombstones.is_empty() {
        return;
    }

    let dialog = adw::AlertDialog::builder()
        .heading("Pre-restore states")
        .body(
            "Each entry below is the live state from before an earlier \
             restore — your safety net for that rollback. The running \
             system itself does not depend on these; deleting them only \
             removes the option to roll back to that earlier state. Once \
             removed, they are gone.",
        )
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("purge", "Purge selected");
    dialog.set_response_appearance("purge", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    // Two-column table: subvolume name + snapshot id, with a
    // checkbox per row. The on-disk subvolume name
    // (`<base>-DELETE-<id>`) is just a wire-level concatenation —
    // composing it back when the user confirms is cheaper than
    // making the user read it. ID is monospace because the
    // YYYYMMDD-HHMMSS-mmm shape only reads cleanly that way.
    let grid = gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(18)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();

    let header_subvol = gtk::Label::builder()
        .label("Subvol")
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();
    let header_id = gtk::Label::builder()
        .label("Snapshot ID")
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();
    grid.attach(&header_subvol, 0, 0, 1, 1);
    grid.attach(&header_id, 1, 0, 1, 1);

    let mut row_checks: Vec<(String, gtk::CheckButton)> = Vec::with_capacity(tombstones.len());
    for (i, t) in tombstones.iter().enumerate() {
        let r = (i as i32) + 1;
        let subvol = gtk::Label::builder()
            .label(&t.base_subvol)
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        let id = gtk::Label::builder()
            .label(&t.id)
            .xalign(0.0)
            .css_classes(["monospace"])
            .hexpand(true)
            .build();
        let check = gtk::CheckButton::builder().active(true).build();
        grid.attach(&subvol, 0, r, 1, 1);
        grid.attach(&id, 1, r, 1, 1);
        grid.attach(&check, 2, r, 1, 1);
        row_checks.push((t.name.clone(), check));
    }
    dialog.set_extra_child(Some(&grid));

    let widgets_for_cb = widgets.clone();
    let state_for_cb = Rc::clone(state);
    let cmd_tx_for_cb = cmd_tx.clone();
    dialog.connect_response(None, move |_dlg, response| {
        if response != "purge" {
            return;
        }
        let selected: Vec<String> = row_checks
            .iter()
            .filter(|(_, c)| c.is_active())
            .map(|(name, _)| name.clone())
            .collect();
        if selected.is_empty() {
            return;
        }

        state_for_cb.borrow_mut().purge_in_flight = true;
        widgets_for_cb.header_btn_cleanup.set_sensitive(false);

        let toast = show_progress_toast(
            &widgets_for_cb.toast_overlay,
            "Purging pre-restore states — waiting for authentication…",
            "Purging pre-restore states…",
        );
        state_for_cb.borrow_mut().purge_progress_toast = Some(toast);

        let _ = cmd_tx_for_cb.send_blocking(Command::PurgeTombstones(selected));
    });

    dialog.present(Some(parent));
}

pub(crate) fn apply_purge_tombstones_result(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    result: Result<Vec<String>, String>,
) {
    {
        let mut st = state.borrow_mut();
        st.purge_in_flight = false;
        if let Some(toast) = st.purge_progress_toast.take() {
            toast.dismiss();
        }
    }
    widgets.header_btn_cleanup.set_sensitive(true);

    match result {
        Ok(removed) => {
            let title = match removed.len() {
                0 => "No pre-restore states removed".to_string(),
                1 => "Removed 1 pre-restore state".to_string(),
                n => format!("Removed {n} pre-restore states"),
            };
            widgets.toast_overlay.add_toast(adw::Toast::new(&title));
            // Button visibility refreshes via DeleteMarkersChanged.
        }
        Err(reason) => {
            tracing::warn!("PurgeDeleteMarkers failed: {reason}");
            widgets
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("Cleanup failed: {reason}")));
        }
    }
}
