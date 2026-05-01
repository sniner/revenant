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

use revenant_core::metadata::format_message_items;

use crate::dbus_thread::{Command, Event, Handles};
use crate::model::{LiveParent, Snapshot, Strain, StrainStats, Tombstone};
use crate::ui::dialogs::create::{apply_create_snapshot_result, present_create_snapshot_dialog};
use crate::ui::dialogs::retention::{apply_retention_result, present_retention_dialog};
use crate::ui::format::{format_created, format_strain_subtitle, kv_pair, show_error_toast};
use crate::ui::toast::{ProgressToast, apply_operation_started, show_progress_toast};

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

/// Build and present the AdwAlertDialog for a Restore action. The
/// dialog has two checkboxes (`save_current`, `dry_run`) following
/// the wireframe; on confirmation it dispatches a `Command::Restore`
/// and hands the in-flight bookkeeping to the result handler.
fn present_restore_dialog(
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

fn apply_restore_result(
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

fn apply_strains(
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

fn select_strain(state: &Rc<RefCell<AppState>>, widgets: &Widgets, name: &str) {
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

fn apply_snapshots(
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
fn refresh_strain_subtitles(widgets: &Widgets, state: &Rc<RefCell<AppState>>) {
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
fn apply_all_snapshots(
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

/// Present the delete-snapshot confirmation. Single-snapshot delete:
/// strain + id are captured from the row's snapshot. Live-anchor rows
/// get an extra warning paragraph so the user knows what they're
/// trading away (the ★ reference, not the running system).
fn present_delete_dialog(
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

/// Refresh the header-bar cleanup button after a `ListDeleteMarkers`
/// reply (initial fetch or `DeleteMarkersChanged` follow-up). Hidden
/// when the list is empty so the header stays clean in the common
/// case; otherwise the button shows the count and a tooltip naming
/// the concept in plain language.
fn apply_tombstones(
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
fn present_cleanup_dialog(
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

fn apply_purge_tombstones_result(
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

/// Reconcile a `SetSnapshotProtected` round-trip with the optimistic
/// icon flip in `snapshot_row`.
///
/// On success the daemon's inotify watcher fires `SnapshotsChanged`,
/// which already triggers a list reload — nothing further is needed
/// here. On failure we surface the daemon's reason as a toast and
/// kick a manual reload of the current strain so the row rebuilds
/// with the (unchanged) on-disk state, undoing the optimistic flip.
fn apply_set_snapshot_protected_result(
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
            if let Some(sel) = selected {
                if sel == strain {
                    let _ = cmd_tx.send_blocking(Command::LoadSnapshots(sel));
                }
            }
        }
    }
}

fn apply_delete_snapshot_result(
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
