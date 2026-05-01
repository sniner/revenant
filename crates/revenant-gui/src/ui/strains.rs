//! Sidebar (strain list) — population, selection, and per-strain
//! stats refresh.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;

use crate::dbus_thread::Command;
use crate::model::{Snapshot, Strain, StrainStats};
use crate::ui::format::{format_strain_subtitle, show_error_toast};
use crate::{AppState, Widgets};

pub(crate) fn apply_strains(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    strains: Vec<Strain>,
) {
    // Stable order from the daemon (sorted by name); reflect it in the
    // sidebar verbatim.
    while let Some(child) = widgets.strain_list.first_child() {
        widgets.strain_list.remove(&child);
    }

    let (live_strain, strain_stats) = {
        let st = state.borrow();
        (
            st.live_parent.as_ref().map(|lp| lp.strain.clone()),
            st.strain_stats.clone(),
        )
    };

    for s in &strains {
        let row = adw::ActionRow::builder().title(s.title()).build();
        let subtitle = format_strain_subtitle(s, strain_stats.get(&s.name));
        if !subtitle.is_empty() {
            row.set_subtitle(&subtitle);
        }
        if live_strain.as_deref() == Some(s.name.as_str()) {
            let pill = gtk::Label::builder()
                .label("★")
                .css_classes(["accent"])
                .build();
            row.add_suffix(&pill);
        }
        widgets.strain_list.append(&row);
    }

    // Preserve the user's selection across refreshes (e.g. when a
    // StrainConfigChanged signal triggers a re-fetch): keep the
    // currently-selected strain if it survived in the new list,
    // otherwise fall back to the daemon's "latest" hint, otherwise
    // the first row. The hint is taken (consumed) so it only steers
    // the very first apply_strains call.
    let prev_selected = state.borrow().selected_strain.clone();
    let initial_pref = state.borrow_mut().initial_pref_strain.take();
    state.borrow_mut().strains = strains.clone();

    let target_idx = prev_selected
        .as_deref()
        .and_then(|sel| strains.iter().position(|s| s.name == sel))
        .or_else(|| {
            initial_pref
                .as_deref()
                .and_then(|sel| strains.iter().position(|s| s.name == sel))
        })
        .or(if strains.is_empty() { None } else { Some(0) });

    match target_idx {
        Some(idx) => {
            if let Some(row) = widgets.strain_list.row_at_index(idx as i32) {
                widgets.strain_list.select_row(Some(&row));
            }
            let target_name = strains[idx].name.clone();
            // `select_row` may not re-emit row-selected if the row
            // was already selected; do the fetch unconditionally so
            // a refresh always reflects the freshest snapshot list
            // and the just-loaded strain config.
            if state.borrow().selected_strain.as_deref() != Some(target_name.as_str()) {
                select_strain(state, widgets, &target_name);
            }
            let _ = cmd_tx.send_blocking(Command::LoadSnapshots(target_name));
        }
        None => {
            widgets
                .snapshot_empty
                .set_description(Some("No strains configured."));
            widgets.snapshot_stack.set_visible_child_name("empty");
            state.borrow_mut().selected_strain = None;
        }
    }
}

pub(crate) fn select_strain(state: &Rc<RefCell<AppState>>, widgets: &Widgets, name: &str) {
    let title = {
        let mut st = state.borrow_mut();
        st.selected_strain = Some(name.to_string());
        // Drop the previous strain's snapshots so a stray late
        // selection callback can't index into a stale list.
        st.snapshots.clear();
        st.strains
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.title().to_string())
            .unwrap_or_else(|| name.to_string())
    };
    widgets.content_title.set_label(&title);
    widgets.snapshot_stack.set_visible_child_name("loading");
}

/// Walk the existing strain rows and rewrite each subtitle from the
/// current `state.strain_stats`. Cheaper than a full apply_strains
/// — keeps row identity, selection, and the ★ suffix in place. Used
/// when an `Event::AllSnapshots` arrives without a strain-list
/// change.
///
/// `AdwActionRow` is itself a `GtkListBoxRow` subclass (via
/// `AdwPreferencesRow`), so `row_at_index` returns the action row
/// directly — downcasting `.child()` would land on the row's
/// internal layout box and fail silently.
pub(crate) fn refresh_strain_subtitles(widgets: &Widgets, state: &Rc<RefCell<AppState>>) {
    let st = state.borrow();
    for (idx, strain) in st.strains.iter().enumerate() {
        let Some(row) = widgets.strain_list.row_at_index(idx as i32) else {
            continue;
        };
        let Ok(action_row) = row.downcast::<adw::ActionRow>() else {
            continue;
        };
        let subtitle = format_strain_subtitle(strain, st.strain_stats.get(&strain.name));
        action_row.set_subtitle(&subtitle);
    }
}

/// Group the cross-strain snapshot list into per-strain stats and
/// refresh sidebar subtitles. Wire-format errors are logged and
/// produce empty stats — better to show "no snapshots yet" briefly
/// than to keep stale data on screen.
pub(crate) fn apply_all_snapshots(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    result: Result<Vec<Snapshot>, String>,
) {
    let snaps = match result {
        Ok(s) => s,
        Err(reason) => {
            tracing::warn!("ListSnapshots(filter={{}}) failed: {reason}");
            state.borrow_mut().strain_stats.clear();
            refresh_strain_subtitles(widgets, state);
            show_error_toast(
                &widgets.toast_overlay,
                "Could not refresh snapshot stats",
                &reason,
            );
            return;
        }
    };

    let mut stats: HashMap<String, StrainStats> = HashMap::new();
    for snap in snaps {
        let entry = stats.entry(snap.strain).or_default();
        entry.count += 1;
        if let Some(iso) = snap.created {
            // RFC 3339 sorts lexicographically by time, so a string
            // compare is fine for "newer than".
            match &entry.latest_iso {
                None => entry.latest_iso = Some(iso),
                Some(prev) if iso > *prev => entry.latest_iso = Some(iso),
                _ => {}
            }
        }
    }
    state.borrow_mut().strain_stats = stats;
    refresh_strain_subtitles(widgets, state);
}
