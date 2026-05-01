//! Small text- and layout-helpers shared between the panes and dialogs.
//!
//! These functions are deliberately stateless and take only their
//! inputs as parameters — they're easy to call from anywhere in the
//! UI tree without dragging `AppState` along.

use adw::prelude::*;
use gtk::glib;

use crate::model::{Snapshot, Strain, StrainStats};

/// Surface a query-failure on the toast overlay. Privileged operations
/// (Restore/Delete/Create/Protect/Cleanup) build their own toasts with
/// operation-specific phrasing; this helper exists for the read-only
/// RPCs (`GetDaemonInfo`, `ListStrains`, `ListSnapshots`,
/// `ListDeleteMarkers`, `GetLiveParent`) so a stale UI does not
/// silently mask a backend error.
pub(crate) fn show_error_toast(overlay: &adw::ToastOverlay, summary: &str, reason: &str) {
    overlay.add_toast(adw::Toast::new(&format!("{summary}: {reason}")));
}

/// Two-column key/value row used inside snapshot detail blocks.
pub(crate) fn kv_pair(key: &str, value: &str) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let k = gtk::Label::builder()
        .label(key)
        .xalign(0.0)
        .width_chars(13)
        .css_classes(["caption-heading", "dim-label"])
        .build();
    let v = gtk::Label::builder()
        .label(value)
        .xalign(0.0)
        .selectable(true)
        .wrap(true)
        .build();
    row.append(&k);
    row.append(&v);
    row
}

/// Build the sidebar subtitle for one strain. Spells the count and
/// the date out — "3 snapshots · 29.4.26 latest" reads on its own
/// without the user having to guess what each fragment means.
/// Combines:
///   - the technical identifier (only when display_name is set;
///     otherwise the title already is the identifier),
///   - the per-strain rollup or "no snapshots yet".
pub(crate) fn format_strain_subtitle(strain: &Strain, stats: Option<&StrainStats>) -> String {
    let identifier = if strain.display_name.is_some() {
        Some(strain.name.as_str())
    } else {
        None
    };
    let rollup = match stats {
        Some(s) if s.count > 0 => {
            let unit = if s.count == 1 {
                "snapshot"
            } else {
                "snapshots"
            };
            let date = s.latest_iso.as_deref().and_then(format_short_date);
            match date {
                Some(d) => format!("{} {unit} · {d} latest", s.count),
                None => format!("{} {unit}", s.count),
            }
        }
        Some(_) | None => "no snapshots yet".to_string(),
    };
    match identifier {
        Some(id) => format!("{id} · {rollup}"),
        None => rollup,
    }
}

/// Compact short-form of a snapshot timestamp for use in the
/// sidebar subtitle ("29.4.26"). Day, month, two-digit year — wide
/// enough to disambiguate across years, narrow enough to fit a
/// 240-px-class sidebar.
pub(crate) fn format_short_date(rfc: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(rfc).ok()?;
    let g = glib::DateTime::from_unix_local(parsed.timestamp()).ok()?;
    Some(format!(
        "{}.{}.{:02}",
        g.day_of_month(),
        g.month(),
        (g.year() % 100).max(0)
    ))
}

/// Render the snapshot's timestamp for display in the row headline.
/// Uses `glib::DateTime` so the locale's translated month name (`%B`)
/// kicks in. Falls back to the raw RFC 3339 if it doesn't parse,
/// then the id.
pub(crate) fn format_created(snap: &Snapshot) -> String {
    let Some(rfc) = snap.created.as_deref() else {
        return snap.id.clone();
    };
    let parsed = match chrono::DateTime::parse_from_rfc3339(rfc) {
        Ok(dt) => dt,
        Err(_) => return rfc.to_string(),
    };
    let Ok(g) = glib::DateTime::from_unix_local(parsed.timestamp()) else {
        return rfc.to_string();
    };
    match g.format("%e. %B %Y, %H:%M:%S") {
        Ok(s) => s.trim().to_string(),
        Err(_) => rfc.to_string(),
    }
}
