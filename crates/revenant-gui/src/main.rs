//! `revenant-gui` — GTK4/libadwaita frontend for the revenant snapshot tool.
//!
//! Two-pane redesign: strain sidebar (with display_name + ★ live
//! marker) and a content area whose snapshot rows carry their own
//! Restore/Delete buttons and a key/value metadata block. The
//! detail pane and the live-state footer were removed — both were
//! redundant with information already visible elsewhere.

mod client;
mod dbus_thread;
mod model;
mod proxy;
mod ui;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::dbus_thread::{Command, Event, Handles};
use crate::model::{LiveParent, Snapshot, Strain, StrainStats, Tombstone};
use crate::ui::dialogs::cleanup::{
    apply_purge_tombstones_result, apply_tombstones, present_cleanup_dialog,
};
use crate::ui::dialogs::create::{apply_create_snapshot_result, present_create_snapshot_dialog};
use crate::ui::dialogs::delete::{
    apply_delete_snapshot_result, apply_set_snapshot_protected_result,
};
use crate::ui::dialogs::restore::apply_restore_result;
use crate::ui::dialogs::retention::{apply_retention_result, present_retention_dialog};
use crate::ui::format::show_error_toast;
use crate::ui::snapshots::apply_snapshots;
use crate::ui::strains::{apply_all_snapshots, apply_strains, select_strain};
use crate::ui::toast::{ProgressToast, apply_operation_started};

const APP_ID: &str = "dev.sniner.RevenantGui";

/// Mutable UI state shared between event handlers and widget callbacks.
/// `Rc<RefCell<...>>` is the standard gtk-rs idiom because GTK is
/// single-threaded — every callback runs on the main loop.
#[derive(Default)]
pub(crate) struct AppState {
    pub(crate) strains: Vec<Strain>,
    pub(crate) live_parent: Option<LiveParent>,
    /// Per-strain rollup ({count, latest_iso}) populated by
    /// `Event::AllSnapshots`. Drives the sidebar subtitle. Read-only
    /// for `apply_strains`; written when the cross-strain snapshot
    /// fetch returns.
    pub(crate) strain_stats: HashMap<String, StrainStats>,
    pub(crate) selected_strain: Option<String>,
    /// Snapshots shown in the centre pane for `selected_strain`.
    /// Indexed by `GtkListBoxRow::index()`.
    pub(crate) snapshots: Vec<Snapshot>,
    /// True between sending `Command::Restore` and receiving
    /// `Event::RestoreResult`. Per-row Restore buttons no-op while
    /// this is set so a second prompt can't queue behind the first.
    pub(crate) restore_in_flight: bool,
    /// Toast displayed while a restore is being processed (polkit
    /// auth + actual subvol work). Held so we can dismiss it the
    /// moment the result arrives, before showing the success toast
    /// or the error toast.
    pub(crate) restore_progress_toast: Option<ProgressToast>,
    /// True between sending `Command::CreateSnapshot` and receiving
    /// `Event::CreateSnapshotResult`. Same purpose as
    /// `restore_in_flight` — gates the strain-header `+` button so a
    /// second click during the polkit prompt doesn't queue a second
    /// snapshot request.
    pub(crate) create_in_flight: bool,
    /// Toast displayed while a CreateSnapshot is being processed.
    /// Same dismissal pattern as `restore_progress_toast`.
    pub(crate) create_progress_toast: Option<ProgressToast>,
    /// True between sending `Command::DeleteSnapshot` and receiving
    /// `Event::DeleteSnapshotResult`. Per-row Delete buttons read
    /// this and no-op while it's set so a second polkit prompt can't
    /// queue behind the first.
    pub(crate) delete_in_flight: bool,
    /// Toast displayed while a DeleteSnapshot is being processed.
    /// Same dismissal pattern as `restore_progress_toast`.
    pub(crate) delete_progress_toast: Option<ProgressToast>,
    /// Strain to pre-select on the very first `Strains` event, sourced
    /// from the daemon's `GetLatestStrain` reply. Consumed (taken) the
    /// first time `apply_strains` runs without an existing user
    /// selection; subsequent refreshes fall back to whatever the user
    /// is currently looking at.
    pub(crate) initial_pref_strain: Option<String>,
    /// Pre-restore states (tombstones) currently on disk. Drives the
    /// header-bar cleanup button's visibility and the contents of the
    /// review dialog.
    pub(crate) tombstones: Vec<Tombstone>,
    /// True between sending `Command::PurgeTombstones` and receiving
    /// `Event::PurgeTombstonesResult`. Gates the cleanup button so a
    /// second polkit prompt can't queue.
    pub(crate) purge_in_flight: bool,
    /// Toast displayed while a purge is being processed.
    pub(crate) purge_progress_toast: Option<ProgressToast>,
}

/// Widget handles the event handlers reach back into. Cloning a GTK
/// widget just bumps a refcount, so this struct can be cloned cheaply
/// into closures.
#[derive(Clone)]
pub(crate) struct Widgets {
    pub(crate) root_stack: gtk::Stack,
    pub(crate) status_page: adw::StatusPage,
    pub(crate) strain_list: gtk::ListBox,
    pub(crate) snapshot_stack: gtk::Stack,
    pub(crate) snapshot_list: gtk::ListBox,
    pub(crate) snapshot_scroll: gtk::ScrolledWindow,
    pub(crate) snapshot_empty: adw::StatusPage,
    pub(crate) snapshot_error: adw::StatusPage,
    /// Title label on the right pane — shows the currently-selected
    /// strain name (display_name preferred, identifier as fallback).
    /// Empty string before the first selection lands.
    pub(crate) content_title: gtk::Label,
    /// Calendar button on the content toolbar — opens the retention
    /// editor for the currently-selected strain.
    pub(crate) strain_btn_retention: gtk::Button,
    /// `+` button on the content toolbar — opens the create-snapshot
    /// dialog. The new snapshot lands in the currently-selected
    /// strain.
    pub(crate) strain_btn_create: gtk::Button,
    /// Tiny icon in the window header showing daemon connection
    /// state. Replaces the old "Live state" footer.
    pub(crate) header_status_icon: gtk::Image,
    /// Header-bar button that surfaces leftover pre-restore states
    /// (DELETE markers). Hidden when there are none. Click opens the
    /// review dialog. The label is the count, so the user sees at a
    /// glance how many entries are waiting.
    pub(crate) header_btn_cleanup: gtk::Button,
    /// Toast overlay wrapping the whole main content. Restore-flow
    /// progress / success / failure messages are surfaced through it.
    pub(crate) toast_overlay: adw::ToastOverlay,
    /// Banner shown after a successful restore with a "Reboot now"
    /// action. Hidden until a real (non-dry-run) restore returns.
    pub(crate) reboot_banner: adw::Banner,
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Revenant")
        .default_width(1100)
        .default_height(720)
        .build();

    let header = adw::HeaderBar::new();
    let header_status_icon = gtk::Image::builder()
        .icon_name("network-offline-symbolic")
        .tooltip_text("Daemon disconnected")
        .build();
    header.pack_end(&header_status_icon);

    // Pre-restore-states cleanup button. Hidden by default; revealed
    // when ListDeleteMarkers returns a non-empty set. The label
    // doubles as the count so the user sees at a glance how many
    // entries are waiting (`dialog-warning-symbolic` carries a hint
    // of yellow in stock icon themes — visible without being alarming).
    let header_btn_cleanup = gtk::Button::builder().visible(false).build();
    let cleanup_btn_content = adw::ButtonContent::builder()
        .icon_name("dialog-warning-symbolic")
        .label("0")
        .build();
    header_btn_cleanup.set_child(Some(&cleanup_btn_content));
    header.pack_start(&header_btn_cleanup);

    // Root stack toggles between the connection-status page and the
    // main UI. Initial child is "connecting" so a slow initial bus
    // connect doesn't flash a half-empty layout at the user.
    let root_stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();

    let status_page = adw::StatusPage::builder()
        .icon_name("drive-harddisk-symbolic")
        .title("Revenant")
        .description("Connecting to revenantd…")
        .vexpand(true)
        .build();
    root_stack.add_named(&status_page, Some("status"));

    let widgets = build_main_ui(&root_stack);

    // Reboot-required banner sits between header and content stack.
    // Hidden by default; revealed on a successful (non-dry-run)
    // restore. Action button kicks off `systemctl reboot` (which
    // routes through logind + the user's desktop polkit agent —
    // standard GNOME pattern, no special handling needed here).
    let reboot_banner = adw::Banner::builder()
        .title("System will boot from the restored snapshot at the next reboot.")
        .button_label("Reboot now")
        .revealed(false)
        .build();

    // Toast overlay wraps the entire main content so toasts appear
    // above whatever's currently on screen, including the connecting
    // status page.
    let toast_overlay = adw::ToastOverlay::new();
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    outer.append(&header);
    outer.append(&reboot_banner);
    outer.append(&root_stack);
    toast_overlay.set_child(Some(&outer));

    let widgets = Widgets {
        root_stack: root_stack.clone(),
        status_page: status_page.clone(),
        toast_overlay: toast_overlay.clone(),
        reboot_banner: reboot_banner.clone(),
        header_status_icon: header_status_icon.clone(),
        header_btn_cleanup: header_btn_cleanup.clone(),
        ..widgets
    };

    window.set_content(Some(&toast_overlay));
    window.present();

    // Reboot banner action: the user has just successfully restored
    // and is ready to reboot. logind's polkit policy normally lets
    // any logged-in graphical user reboot without password — if it
    // doesn't, the spawned process surfaces the polkit prompt.
    {
        reboot_banner.connect_button_clicked(move |_banner| {
            tracing::info!("user clicked Reboot now — invoking systemctl reboot");
            if let Err(e) = std::process::Command::new("systemctl")
                .arg("reboot")
                .spawn()
            {
                tracing::error!("failed to spawn `systemctl reboot`: {e}");
            }
        });
    }

    let handles = dbus_thread::spawn();
    wire_event_loop(window.clone(), widgets, handles);
}

/// Build the sidebar+content layout. Returns the populated `Widgets`
/// (with the wrapper fields like `toast_overlay` and `header_status_icon`
/// patched in by the caller).
fn build_main_ui(root_stack: &gtk::Stack) -> Widgets {
    // ---- sidebar -----------------------------------------------------

    let strain_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();
    let strain_scroll = gtk::ScrolledWindow::builder()
        .child(&strain_list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    // Sidebar gets a small "Strains" heading at the top so the
    // column has a recognisable identity — without it, users see a
    // bare list and have to deduce what they're looking at.
    let sidebar_title = gtk::Label::builder()
        .label("Strains")
        .xalign(0.0)
        .css_classes(["heading"])
        .margin_top(12)
        .margin_bottom(6)
        .margin_start(18)
        .margin_end(18)
        .build();

    let sidebar = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    sidebar.append(&sidebar_title);
    sidebar.append(&strain_scroll);

    // ---- content pane (title + slim toolbar + snapshot list) --------

    // Right pane gets a heading line: the currently-selected strain
    // name on the left, action buttons (retention editor + create
    // snapshot) on the right. The label is set in apply_snapshots
    // when a strain is chosen.
    let content_title = gtk::Label::builder()
        .label("")
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["title-3"])
        .build();

    let strain_btn_retention = gtk::Button::builder()
        .icon_name("x-office-calendar-symbolic")
        .tooltip_text("Edit retention")
        .css_classes(["flat"])
        .build();
    let strain_btn_create = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Create snapshot")
        .css_classes(["flat"])
        .build();

    let toolbar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(12)
        .margin_bottom(6)
        .margin_start(18)
        .margin_end(18)
        .build();
    toolbar.append(&content_title);
    toolbar.append(&strain_btn_retention);
    toolbar.append(&strain_btn_create);

    let snapshot_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["boxed-list"])
        .margin_top(6)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    let snapshot_scroll = gtk::ScrolledWindow::builder()
        .child(&snapshot_list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let snapshot_loading = adw::StatusPage::builder()
        .title("Loading snapshots…")
        .vexpand(true)
        .build();
    let snapshot_empty = adw::StatusPage::builder()
        .icon_name("folder-symbolic")
        .title("No snapshots")
        .description("This strain has no snapshots yet. Use the + button above to create one.")
        .vexpand(true)
        .build();
    let snapshot_error = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Cannot load snapshots")
        .vexpand(true)
        .build();

    let snapshot_stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();
    snapshot_stack.add_named(&snapshot_loading, Some("loading"));
    snapshot_stack.add_named(&snapshot_scroll, Some("list"));
    snapshot_stack.add_named(&snapshot_empty, Some("empty"));
    snapshot_stack.add_named(&snapshot_error, Some("error"));

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content.append(&toolbar);
    content.append(&snapshot_stack);

    // ---- assemble ----------------------------------------------------

    // Two-pane layout: strain sidebar | content. The detail pane
    // and the inner split are gone — snapshot rows carry their own
    // metadata + action buttons.
    let outer_split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar)
        .content(&content)
        .min_sidebar_width(180.0)
        .max_sidebar_width(260.0)
        .build();
    root_stack.add_named(&outer_split, Some("main"));

    Widgets {
        root_stack: root_stack.clone(),
        // Wrapper-layer placeholders overwritten by build_ui.
        status_page: adw::StatusPage::new(),
        toast_overlay: adw::ToastOverlay::new(),
        reboot_banner: adw::Banner::builder().build(),
        header_status_icon: gtk::Image::new(),
        header_btn_cleanup: gtk::Button::new(),
        strain_list,
        snapshot_stack,
        snapshot_list,
        snapshot_scroll,
        snapshot_empty,
        snapshot_error,
        content_title,
        strain_btn_retention,
        strain_btn_create,
    }
}

fn wire_event_loop(window: adw::ApplicationWindow, widgets: Widgets, handles: Handles) {
    let state = Rc::new(RefCell::new(AppState::default()));
    let cmd_tx = handles.commands.clone();

    // Strain selection: send LoadSnapshots whenever the user picks a
    // row. We re-read the strain name from `state.strains` rather than
    // attaching it to the row, because attaching arbitrary data to
    // GtkListBoxRow is awkward and the index is stable within a load.
    {
        let state = Rc::clone(&state);
        let cmd_tx = cmd_tx.clone();
        let widgets_for_cb = widgets.clone();
        widgets.strain_list.connect_row_selected(move |_, row| {
            let Some(row) = row else {
                return;
            };
            let idx = row.index();
            let st = state.borrow();
            if idx < 0 {
                return;
            }
            let Some(strain) = st.strains.get(idx as usize).cloned() else {
                return;
            };
            drop(st);
            select_strain(&state, &widgets_for_cb, &strain.name);
            let _ = cmd_tx.send_blocking(Command::LoadSnapshots(strain.name));
        });
    }

    // Retention editor: open the preferences-style AdwAlertDialog
    // with the current tier values for the selected strain. Save
    // dispatches Command::SetRetention; the result comes back via
    // Event::RetentionResult.
    {
        let state = Rc::clone(&state);
        let cmd_tx = cmd_tx.clone();
        let window_for_cb = window.clone();
        widgets.strain_btn_retention.connect_clicked(move |_| {
            let st = state.borrow();
            let Some(name) = st.selected_strain.clone() else {
                return;
            };
            let Some(strain) = st.strains.iter().find(|s| s.name == name).cloned() else {
                return;
            };
            drop(st);
            present_retention_dialog(&window_for_cb, &cmd_tx, &strain);
        });
    }

    // Create-snapshot: open the dialog with a single optional
    // message field. The strain is implicit (the one in the
    // sidebar). Confirm dispatches Command::CreateSnapshot; result
    // arrives via Event::CreateSnapshotResult.
    {
        let state = Rc::clone(&state);
        let cmd_tx = cmd_tx.clone();
        let widgets_for_cb = widgets.clone();
        let window_for_cb = window.clone();
        widgets.strain_btn_create.connect_clicked(move |_| {
            let st = state.borrow();
            if st.create_in_flight {
                return;
            }
            let Some(name) = st.selected_strain.clone() else {
                return;
            };
            drop(st);
            present_create_snapshot_dialog(&window_for_cb, &widgets_for_cb, &state, &cmd_tx, &name);
        });
    }

    // Cleanup button (header-bar): opens the pre-restore-states review
    // dialog. The button itself stays hidden until ListDeleteMarkers
    // returns a non-empty set.
    {
        let state = Rc::clone(&state);
        let cmd_tx = cmd_tx.clone();
        let widgets_for_cb = widgets.clone();
        let window_for_cb = window.clone();
        widgets.header_btn_cleanup.connect_clicked(move |_| {
            if state.borrow().purge_in_flight {
                return;
            }
            present_cleanup_dialog(&window_for_cb, &widgets_for_cb, &state, &cmd_tx);
        });
    }

    let window_for_events = window;
    let widgets_for_events = widgets;
    let state_for_events = state;
    let cmd_tx_for_events = cmd_tx;
    let events = handles.events;
    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = events.recv().await {
            apply_event(
                &window_for_events,
                &widgets_for_events,
                &state_for_events,
                &cmd_tx_for_events,
                event,
            );
        }
    });
}

fn apply_event(
    parent: &adw::ApplicationWindow,
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    event: Event,
) {
    match event {
        Event::Connected => {
            tracing::info!("daemon connected");
            widgets.root_stack.set_visible_child_name("main");
            widgets
                .header_status_icon
                .set_icon_name(Some("network-transmit-receive-symbolic"));
            widgets
                .header_status_icon
                .set_tooltip_text(Some("Daemon connected"));
        }
        Event::Disconnected(reason) => {
            tracing::warn!("daemon connect failed: {reason}");
            widgets
                .status_page
                .set_description(Some(&format!("Daemon unavailable: {reason}")));
            widgets.root_stack.set_visible_child_name("status");
            widgets
                .header_status_icon
                .set_icon_name(Some("network-offline-symbolic"));
            widgets
                .header_status_icon
                .set_tooltip_text(Some(&format!("Daemon disconnected: {reason}")));
        }
        Event::DaemonInfo(Ok(_)) => {
            // Tooltip-only enrichment; the connection icon already
            // reflects "we're connected" from the Connected event.
        }
        Event::DaemonInfo(Err(reason)) => {
            tracing::warn!("GetDaemonInfo failed: {reason}");
            widgets
                .header_status_icon
                .set_tooltip_text(Some(&format!("Daemon error: {reason}")));
            show_error_toast(&widgets.toast_overlay, "Daemon info unavailable", &reason);
        }
        Event::Strains(Ok(list)) => {
            apply_strains(widgets, state, cmd_tx, list);
        }
        Event::LatestStrain(name) => {
            // Cached for the next apply_strains call; skipped when the
            // wire payload is empty (no snapshots anywhere).
            if !name.is_empty() {
                state.borrow_mut().initial_pref_strain = Some(name);
            }
        }
        Event::Strains(Err(reason)) => {
            tracing::warn!("ListStrains failed: {reason}");
            widgets.snapshot_error.set_description(Some(&reason));
            widgets.snapshot_stack.set_visible_child_name("error");
            show_error_toast(&widgets.toast_overlay, "Could not load strains", &reason);
        }
        Event::LiveParent(Ok(lp)) => {
            // Stash the live parent so the next strain-row rebuild
            // can mark the right strain with ★. The footer is gone;
            // there's nothing else to refresh here.
            state.borrow_mut().live_parent = lp;
            // Trigger a sidebar refresh so the ★ moves immediately,
            // not only on the next StrainConfigChanged event.
            let strains = state.borrow().strains.clone();
            if !strains.is_empty() {
                apply_strains(widgets, state, cmd_tx, strains);
            }
        }
        Event::LiveParent(Err(reason)) => {
            tracing::warn!("GetLiveParent failed: {reason}");
            show_error_toast(
                &widgets.toast_overlay,
                "Could not resolve live parent",
                &reason,
            );
        }
        Event::Snapshots { strain, result } => {
            apply_snapshots(parent, widgets, state, cmd_tx, &strain, result);
        }
        Event::SignalSnapshotsChanged(strain) => {
            // Empty payload from the daemon means "any/all" — reload
            // whichever strain we're currently showing. A specific
            // strain only triggers a reload when it matches; otherwise
            // we'd thrash the off-screen list and lose user time.
            let selected = state.borrow().selected_strain.clone();
            if let Some(sel) = selected {
                if strain.is_empty() || strain == sel {
                    let _ = cmd_tx.send_blocking(Command::LoadSnapshots(sel));
                }
            }
        }
        Event::RestoreResult { strain, id, result } => {
            apply_restore_result(widgets, state, &strain, &id, result);
        }
        Event::RetentionResult { strain, result } => {
            apply_retention_result(widgets, &strain, result);
        }
        Event::CreateSnapshotResult { strain, result } => {
            apply_create_snapshot_result(widgets, state, &strain, result);
        }
        Event::DeleteSnapshotResult { strain, id, result } => {
            apply_delete_snapshot_result(widgets, state, &strain, &id, result);
        }
        Event::SetSnapshotProtectedResult {
            strain,
            id,
            requested,
            result,
        } => {
            apply_set_snapshot_protected_result(
                widgets, state, cmd_tx, &strain, &id, requested, result,
            );
        }
        Event::Tombstones(result) => {
            apply_tombstones(widgets, state, result);
        }
        Event::AllSnapshots(result) => {
            apply_all_snapshots(widgets, state, result);
        }
        Event::OperationStarted => {
            apply_operation_started(state);
        }
        Event::PurgeTombstonesResult(result) => {
            apply_purge_tombstones_result(widgets, state, result);
        }
    }
}
