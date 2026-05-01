//! Retention-editor dialog: six-tier spin-row form, save dispatches a
//! `Command::SetRetention`, the daemon's reply is rendered as a toast.

use adw::prelude::*;

use crate::Widgets;
use crate::dbus_thread::Command;
use crate::model::{Retention, Strain};

/// Build and present the retention-editor dialog for `strain`. Six
/// AdwSpinRows in an AdwPreferencesGroup (one per tier), wrapped in
/// an AdwAlertDialog so we get the standard Cancel/Save button row
/// without hand-rolling a toolbar. Save dispatches a
/// `Command::SetRetention`; the daemon's reply comes back via
/// `Event::RetentionResult` and is rendered as a toast.
pub(crate) fn present_retention_dialog(
    parent: &adw::ApplicationWindow,
    cmd_tx: &async_channel::Sender<Command>,
    strain: &Strain,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Retention — {}", strain.name))
        .body(
            "Snapshots are kept according to tiered policies. A snapshot \
             is retained as long as any tier still claims it. Set a tier \
             to 0 to disable it.",
        )
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");

    // Six tiers in an AdwPreferencesGroup for the rounded-card look
    // that the wireframe sketches. AdwSpinRow takes its bounds via
    // GtkAdjustment. Upper bound 9999 is well past anything sensible
    // and keeps the spin button compact; the daemon clamps anyway.
    let group = adw::PreferencesGroup::builder().build();
    let spin = |label: &str, sub: &str, init: u32| -> adw::SpinRow {
        let adj = gtk::Adjustment::new(f64::from(init), 0.0, 9999.0, 1.0, 10.0, 0.0);
        adw::SpinRow::builder()
            .title(label)
            .subtitle(sub)
            .adjustment(&adj)
            .numeric(true)
            .build()
    };
    let row_last = spin(
        "Last",
        "Most recent snapshots, regardless of age.",
        strain.retention.last,
    );
    let row_hourly = spin(
        "Hourly",
        "Newest per clock-hour for N hours.",
        strain.retention.hourly,
    );
    let row_daily = spin(
        "Daily",
        "Newest per calendar-day for N days.",
        strain.retention.daily,
    );
    let row_weekly = spin(
        "Weekly",
        "Newest per ISO-week for N weeks.",
        strain.retention.weekly,
    );
    let row_monthly = spin(
        "Monthly",
        "Newest per calendar-month for N months.",
        strain.retention.monthly,
    );
    let row_yearly = spin(
        "Yearly",
        "Newest per calendar-year for N years.",
        strain.retention.yearly,
    );
    group.add(&row_last);
    group.add(&row_hourly);
    group.add(&row_daily);
    group.add(&row_weekly);
    group.add(&row_monthly);
    group.add(&row_yearly);

    dialog.set_extra_child(Some(&group));

    let strain_name = strain.name.clone();
    let cmd_tx = cmd_tx.clone();
    dialog.connect_response(None, move |_dlg, response| {
        if response != "save" {
            return;
        }
        let retention = Retention {
            last: row_last.value() as u32,
            hourly: row_hourly.value() as u32,
            daily: row_daily.value() as u32,
            weekly: row_weekly.value() as u32,
            monthly: row_monthly.value() as u32,
            yearly: row_yearly.value() as u32,
        };
        let _ = cmd_tx.send_blocking(Command::SetRetention {
            strain: strain_name.clone(),
            retention,
        });
    });

    dialog.present(Some(parent));
}

pub(crate) fn apply_retention_result(widgets: &Widgets, strain: &str, result: Result<(), String>) {
    match result {
        Ok(()) => {
            widgets
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("Retention saved for {strain}")));
        }
        Err(reason) => {
            tracing::warn!("SetStrainRetention({strain}) failed: {reason}");
            widgets.toast_overlay.add_toast(adw::Toast::new(&format!(
                "Save failed for {strain}: {reason}"
            )));
        }
    }
}
