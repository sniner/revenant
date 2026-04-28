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

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::dbus_thread::{Command, Event, Handles};
use crate::model::{DeleteMarker, LiveParent, Retention, Snapshot, Strain};

const APP_ID: &str = "org.revenant.Gui";

/// Mutable UI state shared between event handlers and widget callbacks.
/// `Rc<RefCell<...>>` is the standard gtk-rs idiom because GTK is
/// single-threaded — every callback runs on the main loop.
#[derive(Default)]
struct AppState {
    strains: Vec<Strain>,
    live_parent: Option<LiveParent>,
    selected_strain: Option<String>,
    /// Snapshots shown in the centre pane for `selected_strain`.
    /// Indexed by `GtkListBoxRow::index()`.
    snapshots: Vec<Snapshot>,
    /// True between sending `Command::Restore` and receiving
    /// `Event::RestoreResult`. Per-row Restore buttons no-op while
    /// this is set so a second prompt can't queue behind the first.
    restore_in_flight: bool,
    /// Toast displayed while a restore is being processed (polkit
    /// auth + actual subvol work). Held so we can dismiss it the
    /// moment the result arrives, before showing the success toast
    /// or the error toast.
    restore_progress_toast: Option<adw::Toast>,
    /// True between sending `Command::CreateSnapshot` and receiving
    /// `Event::CreateSnapshotResult`. Same purpose as
    /// `restore_in_flight` — gates the strain-header `+` button so a
    /// second click during the polkit prompt doesn't queue a second
    /// snapshot request.
    create_in_flight: bool,
    /// Toast displayed while a CreateSnapshot is being processed.
    /// Same dismissal pattern as `restore_progress_toast`.
    create_progress_toast: Option<adw::Toast>,
    /// True between sending `Command::DeleteSnapshot` and receiving
    /// `Event::DeleteSnapshotResult`. Per-row Delete buttons read
    /// this and no-op while it's set so a second polkit prompt can't
    /// queue behind the first.
    delete_in_flight: bool,
    /// Toast displayed while a DeleteSnapshot is being processed.
    /// Same dismissal pattern as `restore_progress_toast`.
    delete_progress_toast: Option<adw::Toast>,
    /// Strain to pre-select on the very first `Strains` event, sourced
    /// from the daemon's `GetLatestStrain` reply. Consumed (taken) the
    /// first time `apply_strains` runs without an existing user
    /// selection; subsequent refreshes fall back to whatever the user
    /// is currently looking at.
    initial_pref_strain: Option<String>,
    /// Pre-restore states (DELETE markers) currently on disk. Drives
    /// the header-bar cleanup button's visibility and the contents of
    /// the review dialog.
    delete_markers: Vec<DeleteMarker>,
    /// True between sending `Command::PurgeDeleteMarkers` and receiving
    /// `Event::PurgeDeleteMarkersResult`. Gates the cleanup button so
    /// a second polkit prompt can't queue.
    purge_in_flight: bool,
    /// Toast displayed while a purge is being processed.
    purge_progress_toast: Option<adw::Toast>,
}

/// Widget handles the event handlers reach back into. Cloning a GTK
/// widget just bumps a refcount, so this struct can be cloned cheaply
/// into closures.
#[derive(Clone)]
struct Widgets {
    root_stack: gtk::Stack,
    status_page: adw::StatusPage,
    strain_list: gtk::ListBox,
    snapshot_stack: gtk::Stack,
    snapshot_list: gtk::ListBox,
    snapshot_scroll: gtk::ScrolledWindow,
    snapshot_empty: adw::StatusPage,
    snapshot_error: adw::StatusPage,
    /// Calendar button on the content toolbar — opens the retention
    /// editor for the currently-selected strain.
    strain_btn_retention: gtk::Button,
    /// `+` button on the content toolbar — opens the create-snapshot
    /// dialog. The new snapshot lands in the currently-selected
    /// strain.
    strain_btn_create: gtk::Button,
    /// Tiny icon in the window header showing daemon connection
    /// state. Replaces the old "Live state" footer.
    header_status_icon: gtk::Image,
    /// Header-bar button that surfaces leftover pre-restore states
    /// (DELETE markers). Hidden when there are none. Click opens the
    /// review dialog. The label is the count, so the user sees at a
    /// glance how many entries are waiting.
    header_btn_cleanup: gtk::Button,
    /// Toast overlay wrapping the whole main content. Restore-flow
    /// progress / success / failure messages are surfaced through it.
    toast_overlay: adw::ToastOverlay,
    /// Banner shown after a successful restore with a "Reboot now"
    /// action. Hidden until a real (non-dry-run) restore returns.
    reboot_banner: adw::Banner,
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

    let sidebar = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    sidebar.append(&strain_scroll);

    // ---- content pane (slim toolbar + snapshot list) ----------------

    // Strain-scoped action buttons: calendar = retention editor,
    // `+` = create snapshot. The previous title label was redundant
    // — the strain is already highlighted in the sidebar — so the
    // toolbar is buttons-only, right-aligned via a hexpand spacer.
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
    let spacer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .hexpand(true)
        .build();
    toolbar.append(&spacer);
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
        .min_sidebar_width(220.0)
        .max_sidebar_width(320.0)
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
        Event::DeleteMarkers(result) => {
            apply_delete_markers(widgets, state, result);
        }
        Event::PurgeDeleteMarkersResult(result) => {
            apply_purge_delete_markers_result(widgets, state, result);
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

        let progress_label = if dry_run {
            "Running preflight checks…"
        } else {
            "Restoring snapshot — waiting for authentication…"
        };
        let toast = adw::Toast::builder()
            .title(progress_label)
            .timeout(0) // 0 = do not auto-dismiss; cleared by RestoreResult
            .build();
        widgets.toast_overlay.add_toast(toast.clone());
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

    let live_strain = state
        .borrow()
        .live_parent
        .as_ref()
        .map(|lp| lp.strain.clone());

    for s in &strains {
        let row = adw::ActionRow::builder().title(s.title()).build();
        // Show the technical identifier as a subtitle only when a
        // display_name is present — otherwise the title already is
        // the identifier and a duplicated subtitle is just noise.
        if s.display_name.is_some() {
            row.set_subtitle(&s.name);
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
    {
        let mut st = state.borrow_mut();
        st.selected_strain = Some(name.to_string());
        // Drop the previous strain's snapshots so a stray late
        // selection callback can't index into a stale list.
        st.snapshots.clear();
    }
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

    let restore_btn = gtk::Button::builder()
        .label("Restore")
        .css_classes(["pill"])
        .build();
    let delete_btn = gtk::Button::builder()
        .label("Delete")
        .css_classes(["pill", "destructive-action"])
        .build();
    if snap.is_live_anchor {
        delete_btn.set_tooltip_text(Some(
            "This snapshot is the parent of the running system. \
             Deleting it removes the ★ live-anchor reference; the \
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
        .spacing(2)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(&headline);
    if kv.first_child().is_some() {
        body.append(&kv);
    }

    let row = gtk::ListBoxRow::builder().child(&body).build();

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

fn kv_pair(key: &str, value: &str) -> gtk::Box {
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

/// Join a snapshot's metadata `message` into a human-readable summary,
/// truncating long lists to keep the line scannable. Returns `None`
/// for an empty list so callers can suppress the row entirely.
fn format_message_items(items: &[String]) -> Option<String> {
    match items.len() {
        0 => None,
        1..=3 => Some(items.join(", ")),
        _ => Some(format!("{}, {}, +{}", items[0], items[1], items.len() - 2)),
    }
}

/// Render the snapshot's timestamp for display in the row headline.
/// Uses `glib::DateTime` so the locale's translated month name (`%B`)
/// kicks in. Falls back to the raw RFC 3339 if it doesn't parse,
/// then the id.
fn format_created(snap: &Snapshot) -> String {
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

/// Build and present the retention-editor dialog for `strain`. Six
/// AdwSpinRows in an AdwPreferencesGroup (one per tier), wrapped in
/// an AdwAlertDialog so we get the standard Cancel/Save button row
/// without hand-rolling a toolbar. Save dispatches a
/// `Command::SetRetention`; the daemon's reply comes back via
/// `Event::RetentionResult` and is rendered as a toast.
fn present_retention_dialog(
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

    // Contextual footgun warning. Visible only when `last == 0` and
    // any longer tier is active — matches the same-day-eviction edge
    // case kept-by-design (see project memory). Re-evaluated whenever
    // any spinner changes.
    let warning = gtk::Label::builder()
        .label(
            "⚠ With Last = 0 and only longer tiers active, a same-day \
             pre-restore snapshot can evict an older same-day pick.",
        )
        .wrap(true)
        .xalign(0.0)
        .css_classes(["caption", "warning"])
        .margin_top(8)
        .visible(false)
        .build();

    let extra = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    extra.append(&group);
    extra.append(&warning);
    dialog.set_extra_child(Some(&extra));

    // Reactive footgun visibility. Each spin row reports value
    // changes via notify::value; clone the row handles into the
    // closure so we can read them all on each tick. Initial state is
    // computed once after construction.
    {
        // Hourly is intentionally not part of the footgun rule
        // (same-second/hour eviction isn't the failure mode the
        // warning is about); only daily and longer tiers matter.
        let r_last = row_last.clone();
        let r_daily = row_daily.clone();
        let r_weekly = row_weekly.clone();
        let r_monthly = row_monthly.clone();
        let r_yearly = row_yearly.clone();
        let warning = warning.clone();
        let recompute = move || {
            let trip = r_last.value() as u32 == 0
                && (r_daily.value() as u32 > 0
                    || r_weekly.value() as u32 > 0
                    || r_monthly.value() as u32 > 0
                    || r_yearly.value() as u32 > 0);
            warning.set_visible(trip);
        };
        recompute();
        for row in [
            &row_last,
            &row_hourly,
            &row_daily,
            &row_weekly,
            &row_monthly,
            &row_yearly,
        ] {
            let cb = recompute.clone();
            row.connect_notify_local(Some("value"), move |_, _| cb());
        }
    }

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

fn apply_retention_result(widgets: &Widgets, strain: &str, result: Result<(), String>) {
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

/// Present the create-snapshot dialog. AdwAlertDialog with one
/// AdwEntryRow for the optional message; strain is implicit (from
/// the header, passed in as `strain`). Trigger is implicitly
/// `manual` — the daemon hardcodes that for D-Bus CreateSnapshot.
fn present_create_snapshot_dialog(
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

        let toast = adw::Toast::builder()
            .title("Creating snapshot — waiting for authentication…")
            .timeout(0)
            .build();
        widgets_for_cb.toast_overlay.add_toast(toast.clone());
        state_for_cb.borrow_mut().create_progress_toast = Some(toast);

        let _ = cmd_tx_for_cb.send_blocking(Command::CreateSnapshot {
            strain: strain_for_cb.clone(),
            message,
        });
    });

    dialog.present(Some(parent));
}

fn apply_create_snapshot_result(
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

        let toast = adw::Toast::builder()
            .title("Deleting snapshot — waiting for authentication…")
            .timeout(0)
            .build();
        widgets.toast_overlay.add_toast(toast.clone());
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
fn apply_delete_markers(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    result: Result<Vec<DeleteMarker>, String>,
) {
    let markers = match result {
        Ok(m) => m,
        Err(reason) => {
            tracing::warn!("ListDeleteMarkers failed: {reason}");
            Vec::new()
        }
    };
    let count = markers.len();
    state.borrow_mut().delete_markers = markers;

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
/// per `DeleteMarker`, each with its own checkbox; default is "all
/// checked" so the typical case (user opens, hits Purge) is one
/// click. Confirm dispatches `Command::PurgeDeleteMarkers`; the
/// result arrives via `Event::PurgeDeleteMarkersResult`.
fn present_cleanup_dialog(
    parent: &adw::ApplicationWindow,
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
) {
    let markers = state.borrow().delete_markers.clone();
    if markers.is_empty() {
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

    // One AdwActionRow per marker, with a CheckButton suffix that
    // doubles as the row's activation widget — clicking anywhere on
    // the row toggles the checkbox.
    let group = adw::PreferencesGroup::builder().build();
    let mut row_checks: Vec<(String, gtk::CheckButton)> = Vec::with_capacity(markers.len());
    for m in &markers {
        let row = adw::ActionRow::builder().title(&m.name).build();
        if let Some(created) = format_marker_created(m) {
            row.set_subtitle(&format!("{} · from {}", m.base_subvol, created));
        } else {
            row.set_subtitle(&m.base_subvol);
        }
        let check = gtk::CheckButton::builder().active(true).build();
        row.add_suffix(&check);
        row.set_activatable_widget(Some(&check));
        group.add(&row);
        row_checks.push((m.name.clone(), check));
    }
    dialog.set_extra_child(Some(&group));

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

        let toast = adw::Toast::builder()
            .title("Purging pre-restore states — waiting for authentication…")
            .timeout(0)
            .build();
        widgets_for_cb.toast_overlay.add_toast(toast.clone());
        state_for_cb.borrow_mut().purge_progress_toast = Some(toast);

        let _ = cmd_tx_for_cb.send_blocking(Command::PurgeDeleteMarkers(selected));
    });

    dialog.present(Some(parent));
}

fn apply_purge_delete_markers_result(
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

/// Format a marker's `created` timestamp for the row subtitle. Falls
/// back to the raw RFC 3339 if it doesn't parse, then `None` so the
/// caller drops the date entirely.
fn format_marker_created(m: &DeleteMarker) -> Option<String> {
    let rfc = m.created.as_deref()?;
    let parsed = chrono::DateTime::parse_from_rfc3339(rfc).ok()?;
    let g = glib::DateTime::from_unix_local(parsed.timestamp()).ok()?;
    g.format("%e. %B %Y, %H:%M:%S")
        .ok()
        .map(|s| s.trim().to_string())
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
