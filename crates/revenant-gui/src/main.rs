//! `revenant-gui` — GTK4/libadwaita frontend for the revenant snapshot tool.
//!
//! Slice state: 4d — Phase 1 GUI is feature-complete: strain
//! sidebar, snapshot list, detail pane, strain-header action
//! buttons (retention editor wired; create-snapshot still
//! pending), live signal subscriptions, privileged Restore flow,
//! and per-strain retention editor (this slice). Layout follows
//! `docs/design/gui-wireframes.md`.

mod client;
mod dbus_thread;
mod model;
mod proxy;

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::dbus_thread::{Command, Event, Handles};
use crate::model::{LiveParent, Retention, Snapshot, Strain};
use crate::proxy::Dict;

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
    /// Indexed by `GtkListBoxRow::index()`, so the row-selection
    /// callback can resolve a click back to the model entry.
    snapshots: Vec<Snapshot>,
    /// Id of the snapshot currently shown in the detail pane, or
    /// `None` when no row is selected. Re-asserted after each list
    /// reload so a refresh that doesn't drop the previously selected
    /// snapshot keeps the pane populated.
    selected_snapshot: Option<String>,
    /// True between sending `Command::Restore` and receiving
    /// `Event::RestoreResult`. Disables the Restore button so the
    /// user cannot stack a second request behind a polkit prompt.
    restore_in_flight: bool,
    /// Toast displayed while a restore is being processed (polkit
    /// auth + actual subvol work). Held so we can dismiss it the
    /// moment the result arrives, before showing the success toast
    /// or the error toast.
    restore_progress_toast: Option<adw::Toast>,
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
    snapshot_empty: adw::StatusPage,
    snapshot_error: adw::StatusPage,
    content_title: gtk::Label,
    footer_state: gtk::Label,
    footer_live: gtk::Label,
    footer_live_detail: gtk::Label,
    /// Detail pane (right side of the inner OverlaySplitView). The
    /// stack toggles between an empty placeholder and the populated
    /// detail layout.
    detail_stack: gtk::Stack,
    detail_title: gtk::Label,
    detail_subtitle: gtk::Label,
    detail_message: gtk::Label,
    detail_trigger: gtk::Label,
    detail_subvols: gtk::Label,
    detail_created: gtk::Label,
    detail_pill_protected: gtk::Label,
    detail_pill_anchor: gtk::Label,
    detail_restore_btn: gtk::Button,
    /// `✎` button on the strain header — opens the retention editor.
    strain_btn_retention: gtk::Button,
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
/// (with the two stack/status_page fields filled in by the caller —
/// they belong to the root stack, not the main UI).
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

    // Live-state footer block. Pinned at the bottom of the sidebar.
    let footer_state = gtk::Label::builder()
        .label("…")
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();
    let footer_heading = gtk::Label::builder()
        .label("Live state")
        .xalign(0.0)
        .css_classes(["caption-heading", "dim-label"])
        .build();
    let footer_live = gtk::Label::builder()
        .label("Pristine — no restore yet.")
        .xalign(0.0)
        .wrap(true)
        .build();
    let footer_live_detail = gtk::Label::builder()
        .label("")
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();

    let footer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    footer.append(&footer_state);
    footer.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    footer.append(&footer_heading);
    footer.append(&footer_live);
    footer.append(&footer_live_detail);

    let sidebar = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    sidebar.append(&strain_scroll);
    sidebar.append(&footer);

    // ---- centre pane (strain header + snapshot list) ----------------

    let content_title = gtk::Label::builder()
        .label("")
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["title-2"])
        .build();

    // Strain-scoped action buttons. Both icon-only with tooltip per
    // GNOME HIG; placed in the strain header (not the app header bar)
    // because they act on the currently-selected strain. Wired in
    // later slices: `+` opens the create-snapshot dialog, `✎` opens
    // the retention editor.
    let strain_btn_retention = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .tooltip_text("Edit retention")
        .css_classes(["flat"])
        .build();
    let strain_btn_create = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Create snapshot")
        .css_classes(["flat"])
        .build();

    let strain_header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(18)
        .margin_bottom(12)
        .margin_start(18)
        .margin_end(18)
        .build();
    strain_header.append(&content_title);
    strain_header.append(&strain_btn_retention);
    strain_header.append(&strain_btn_create);

    let snapshot_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["boxed-list"])
        .margin_top(0)
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

    let centre = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    centre.append(&strain_header);
    centre.append(&snapshot_stack);

    // ---- detail pane -------------------------------------------------

    let detail_widgets = build_detail_pane();

    // ---- assemble ----------------------------------------------------

    // Three-pane layout via two nested OverlaySplitViews:
    //   outer:  strain sidebar  |  inner-split
    //   inner:  centre pane     |  detail pane
    // Each split collapses independently on narrow widths.
    let inner_split = adw::OverlaySplitView::builder()
        .sidebar_position(gtk::PackType::End)
        .sidebar(&detail_widgets.root)
        .content(&centre)
        .min_sidebar_width(280.0)
        .max_sidebar_width(380.0)
        .build();

    let outer_split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar)
        .content(&inner_split)
        .min_sidebar_width(220.0)
        .max_sidebar_width(320.0)
        .build();
    root_stack.add_named(&outer_split, Some("main"));

    Widgets {
        root_stack: root_stack.clone(),
        // The next four are placeholders overwritten by build_ui; they
        // belong to the toast-overlay/banner/window layer wrapping the
        // main UI and aren't built here.
        status_page: adw::StatusPage::new(),
        toast_overlay: adw::ToastOverlay::new(),
        reboot_banner: adw::Banner::builder().build(),
        strain_list,
        snapshot_stack,
        snapshot_list,
        snapshot_empty,
        snapshot_error,
        content_title,
        footer_state,
        footer_live,
        footer_live_detail,
        detail_stack: detail_widgets.stack,
        detail_title: detail_widgets.title,
        detail_subtitle: detail_widgets.subtitle,
        detail_message: detail_widgets.message,
        detail_trigger: detail_widgets.trigger,
        detail_subvols: detail_widgets.subvols,
        detail_created: detail_widgets.created,
        detail_pill_protected: detail_widgets.pill_protected,
        detail_pill_anchor: detail_widgets.pill_anchor,
        detail_restore_btn: detail_widgets.restore_btn,
        strain_btn_retention,
    }
}

/// Internal handle returned by [`build_detail_pane`]. The fields are
/// flattened into [`Widgets`] by the caller; grouping them here just
/// keeps `build_main_ui` from drowning in tuple destructuring.
struct DetailWidgets {
    root: gtk::Box,
    stack: gtk::Stack,
    title: gtk::Label,
    subtitle: gtk::Label,
    message: gtk::Label,
    trigger: gtk::Label,
    subvols: gtk::Label,
    created: gtk::Label,
    pill_protected: gtk::Label,
    pill_anchor: gtk::Label,
    restore_btn: gtk::Button,
}

/// Build the right-hand snapshot detail pane. Two children in a
/// stack: an empty placeholder shown when nothing is selected, and
/// the populated layout populated by [`apply_detail`] when a row is
/// clicked.
fn build_detail_pane() -> DetailWidgets {
    let placeholder = adw::StatusPage::builder()
        .icon_name("view-paged-symbolic")
        .title("No snapshot selected")
        .description("Select a snapshot to see its details.")
        .vexpand(true)
        .build();

    // ---- populated layout -------------------------------------------

    let title = gtk::Label::builder()
        .label("")
        .xalign(0.0)
        .wrap(true)
        .css_classes(["title-2"])
        .build();
    let subtitle = gtk::Label::builder()
        .label("")
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();

    let message = gtk::Label::builder()
        .label("")
        .xalign(0.0)
        .wrap(true)
        .css_classes(["body"])
        .margin_top(8)
        .build();

    // Two pills shown side-by-side under the message; each is
    // hidden via `set_visible(false)` when its condition doesn't
    // hold for the selected snapshot.
    let pill_anchor = gtk::Label::builder()
        .label("★ Anchor")
        .css_classes(["caption-heading", "accent"])
        .visible(false)
        .build();
    let pill_protected = gtk::Label::builder()
        .label("Protected")
        .css_classes(["caption-heading", "dim-label"])
        .visible(false)
        .build();
    let pills = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(8)
        .build();
    pills.append(&pill_anchor);
    pills.append(&pill_protected);

    // K/V grid for trigger / subvols / created. AdwActionRow would
    // be too heavy here (snapshot details are small, dense, and
    // read-only); a plain Grid with caption-styled keys keeps the
    // pane visually quiet.
    let trigger = make_kv_value();
    let subvols = make_kv_value();
    let created = make_kv_value();

    let grid = gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(18)
        .margin_top(18)
        .build();
    grid.attach(&kv_label("Trigger"), 0, 0, 1, 1);
    grid.attach(&trigger, 1, 0, 1, 1);
    grid.attach(&kv_label("Subvols"), 0, 1, 1, 1);
    grid.attach(&subvols, 1, 1, 1, 1);
    grid.attach(&kv_label("Created"), 0, 2, 1, 1);
    grid.attach(&created, 1, 2, 1, 1);

    // Restore button: enabled by `apply_detail` whenever a snapshot
    // is shown; click handler attached in `wire_event_loop` so it
    // can capture state + cmd_tx + parent-window references.
    let restore_btn = gtk::Button::builder()
        .label("Restore…")
        .css_classes(["suggested-action", "pill"])
        .sensitive(false)
        .build();
    let delete_btn = gtk::Button::builder()
        .label("Delete")
        .css_classes(["destructive-action", "flat"])
        .sensitive(false)
        .tooltip_text("Phase 2 — not yet implemented")
        .build();
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(24)
        .halign(gtk::Align::Start)
        .build();
    actions.append(&restore_btn);
    actions.append(&delete_btn);

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    inner.append(&title);
    inner.append(&subtitle);
    inner.append(&message);
    inner.append(&pills);
    inner.append(&grid);
    inner.append(&actions);

    let inner_scroll = gtk::ScrolledWindow::builder()
        .child(&inner)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();
    stack.add_named(&placeholder, Some("empty"));
    stack.add_named(&inner_scroll, Some("populated"));
    stack.set_visible_child_name("empty");

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    root.append(&stack);

    DetailWidgets {
        root,
        stack,
        title,
        subtitle,
        message,
        trigger,
        subvols,
        created,
        pill_protected,
        pill_anchor,
        restore_btn,
    }
}

fn kv_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["caption-heading", "dim-label"])
        .build()
}

fn make_kv_value() -> gtk::Label {
    gtk::Label::builder()
        .label("")
        .xalign(0.0)
        .selectable(true)
        .wrap(true)
        .build()
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

    // Snapshot row selection → populate the detail pane. The model
    // entry comes from `state.snapshots`; row.index() is stable
    // within a single populated list (apply_snapshots rebuilds the
    // whole list whenever the model changes).
    {
        let state = Rc::clone(&state);
        let widgets_for_cb = widgets.clone();
        widgets
            .snapshot_list
            .connect_row_selected(move |_, row| match row {
                Some(row) => {
                    let idx = row.index();
                    if idx < 0 {
                        clear_detail(&widgets_for_cb, &state);
                        return;
                    }
                    let st = state.borrow();
                    let Some(snap) = st.snapshots.get(idx as usize).cloned() else {
                        return;
                    };
                    let strain_subvols = strain_subvols_for(&st, &snap.strain);
                    drop(st);
                    state.borrow_mut().selected_snapshot = Some(snap.id.clone());
                    apply_detail(&widgets_for_cb, &snap, &strain_subvols);
                }
                None => clear_detail(&widgets_for_cb, &state),
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

    // Restore button: open the AdwAlertDialog. Confirmation handler
    // dispatches Command::Restore; the actual restore is async and
    // comes back via Event::RestoreResult. Polkit prompt happens
    // inside the daemon while we're awaiting the call.
    {
        let state = Rc::clone(&state);
        let cmd_tx = cmd_tx.clone();
        let widgets_for_cb = widgets.clone();
        let window_for_cb = window.clone();
        widgets.detail_restore_btn.connect_clicked(move |_| {
            let st = state.borrow();
            if st.restore_in_flight {
                return;
            }
            let Some(snap_id) = st.selected_snapshot.clone() else {
                return;
            };
            let Some(snap) = st.snapshots.iter().find(|s| s.id == snap_id).cloned() else {
                return;
            };
            drop(st);
            present_restore_dialog(&window_for_cb, &widgets_for_cb, &state, &cmd_tx, &snap);
        });
    }

    let widgets_for_events = widgets;
    let state_for_events = state;
    let cmd_tx_for_events = cmd_tx;
    let events = handles.events;
    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = events.recv().await {
            apply_event(
                &widgets_for_events,
                &state_for_events,
                &cmd_tx_for_events,
                event,
            );
        }
    });
}

fn apply_event(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    event: Event,
) {
    match event {
        Event::Connected => {
            tracing::info!("daemon connected");
            widgets.root_stack.set_visible_child_name("main");
        }
        Event::Disconnected(reason) => {
            tracing::warn!("daemon connect failed: {reason}");
            widgets
                .status_page
                .set_description(Some(&format!("Daemon unavailable: {reason}")));
            widgets.root_stack.set_visible_child_name("status");
        }
        Event::DaemonInfo(Ok(info)) => {
            widgets.footer_state.set_label(&summarize_info(&info));
        }
        Event::DaemonInfo(Err(reason)) => {
            tracing::warn!("GetDaemonInfo failed: {reason}");
            widgets
                .footer_state
                .set_label(&format!("daemon error: {reason}"));
        }
        Event::Strains(Ok(list)) => {
            apply_strains(widgets, state, cmd_tx, list);
        }
        Event::Strains(Err(reason)) => {
            tracing::warn!("ListStrains failed: {reason}");
            widgets.snapshot_error.set_description(Some(&reason));
            widgets.snapshot_stack.set_visible_child_name("error");
        }
        Event::LiveParent(Ok(lp)) => {
            state.borrow_mut().live_parent = lp;
            apply_live_parent(widgets, state);
        }
        Event::LiveParent(Err(reason)) => {
            tracing::warn!("GetLiveParent failed: {reason}");
            widgets.footer_live.set_label("Live parent unknown.");
            widgets
                .footer_live_detail
                .set_label(&format!("error: {reason}"));
        }
        Event::Snapshots { strain, result } => {
            apply_snapshots(widgets, state, &strain, result);
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
        format_created_full(snap),
        snap.message.as_deref().unwrap_or("(no message)"),
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
        // assume the start state when it clears the flag.
        {
            let mut st = state.borrow_mut();
            st.restore_in_flight = true;
        }
        widgets.detail_restore_btn.set_sensitive(false);

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
    // Re-enable the Restore button when there's still a selection.
    if state.borrow().selected_snapshot.is_some() {
        widgets.detail_restore_btn.set_sensitive(true);
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
        let row = adw::ActionRow::builder().title(&s.name).build();
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
    // otherwise fall back to the first.
    let prev_selected = state.borrow().selected_strain.clone();
    state.borrow_mut().strains = strains.clone();

    let target_idx = prev_selected
        .as_deref()
        .and_then(|sel| strains.iter().position(|s| s.name == sel))
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
            widgets.content_title.set_label("");
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
        st.selected_snapshot = None;
    }
    widgets.content_title.set_label(name);
    widgets.snapshot_stack.set_visible_child_name("loading");
    widgets.detail_stack.set_visible_child_name("empty");
}

fn apply_snapshots(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    strain: &str,
    result: Result<Vec<Snapshot>, String>,
) {
    // Race protection: if the user has switched strain since this
    // request was sent, drop the result on the floor.
    if state.borrow().selected_strain.as_deref() != Some(strain) {
        return;
    }

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
            widgets.detail_stack.set_visible_child_name("empty");
        }
        Ok(snaps) => {
            // Daemon sorts oldest-first by id; reverse for newest-on-top
            // display matching the wireframes. Mirror the displayed
            // order in `state.snapshots` so the row-selection handler
            // can resolve a click via row.index().
            let ordered: Vec<Snapshot> = snaps.into_iter().rev().collect();
            for snap in &ordered {
                widgets.snapshot_list.append(&snapshot_row(snap));
            }

            // Preserve detail-pane selection across reloads when the
            // previously selected snapshot is still in the list.
            let prev_id = state.borrow().selected_snapshot.clone();
            let restore_idx = prev_id
                .as_deref()
                .and_then(|id| ordered.iter().position(|s| s.id == id));

            state.borrow_mut().snapshots = ordered;
            widgets.snapshot_stack.set_visible_child_name("list");

            if let Some(idx) = restore_idx {
                if let Some(row) = widgets.snapshot_list.row_at_index(idx as i32) {
                    widgets.snapshot_list.select_row(Some(&row));
                }
            } else {
                widgets.snapshot_list.unselect_all();
                widgets.detail_stack.set_visible_child_name("empty");
                state.borrow_mut().selected_snapshot = None;
            }
        }
        Err(reason) => {
            state.borrow_mut().snapshots.clear();
            tracing::warn!("ListSnapshots({strain}) failed: {reason}");
            widgets.snapshot_error.set_description(Some(&reason));
            widgets.snapshot_stack.set_visible_child_name("error");
            widgets.detail_stack.set_visible_child_name("empty");
        }
    }
}

fn snapshot_row(snap: &Snapshot) -> adw::ActionRow {
    let title = format_created(snap);
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&title).as_str())
        .subtitle(snap.message.as_deref().unwrap_or(""))
        .build();

    // Trigger pill on the left side. Keeps the row visually compact;
    // detail pane (4c) will render the full info.
    let trigger = gtk::Label::builder()
        .label(&snap.trigger)
        .css_classes(["caption", "dim-label"])
        .build();
    row.add_prefix(&trigger);

    if snap.is_live_anchor {
        let pill = gtk::Label::builder()
            .label("★ anchor")
            .css_classes(["accent", "caption-heading"])
            .build();
        row.add_suffix(&pill);
    }
    row
}

/// Render the snapshot's timestamp for display. Falls back to the id
/// itself when the daemon couldn't supply a parseable `created`.
fn format_created(snap: &Snapshot) -> String {
    match &snap.created {
        Some(rfc) => match chrono::DateTime::parse_from_rfc3339(rfc) {
            Ok(dt) => dt
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            Err(_) => rfc.clone(),
        },
        None => snap.id.clone(),
    }
}

fn apply_live_parent(widgets: &Widgets, state: &Rc<RefCell<AppState>>) {
    let st = state.borrow();
    match &st.live_parent {
        Some(lp) => {
            widgets
                .footer_live
                .set_label(&format!("{} on {}", lp.strain, lp.id));
            widgets.footer_live_detail.set_label("");
        }
        None => {
            widgets.footer_live.set_label("Pristine — no restore yet.");
            widgets.footer_live_detail.set_label("");
        }
    }
}

/// Compact one-line summary of the daemon-info dict for the sidebar
/// footer. Slice 4f will turn this into a live-updating reflection
/// of `DaemonStateChanged`.
fn summarize_info(info: &Dict) -> String {
    let version = read_str(info, "version").unwrap_or("?");
    let mounted = read_bool(info, "toplevel_mounted").unwrap_or(false);
    if mounted {
        format!("revenantd {version} • ready")
    } else {
        let reason = read_str(info, "degraded_reason").unwrap_or("unknown");
        format!("revenantd {version} • degraded: {reason}")
    }
}

fn read_str<'a>(dict: &'a Dict, key: &str) -> Option<&'a str> {
    dict.get(key).and_then(|v| <&str>::try_from(v).ok())
}

fn read_bool(dict: &Dict, key: &str) -> Option<bool> {
    dict.get(key).and_then(|v| bool::try_from(v).ok())
}

/// Look up the configured subvolumes for `strain` in the cached
/// strain list. Note: this reflects the *current* strain config,
/// which can drift from what was actually snapshotted at the time
/// (snapshot doesn't carry its own subvolume manifest yet — when
/// it does, swap this for that data). Returns an empty slice when
/// the strain is unknown.
fn strain_subvols_for(state: &AppState, strain: &str) -> Vec<String> {
    state
        .strains
        .iter()
        .find(|s| s.name == strain)
        .map(|s| s.subvolumes.clone())
        .unwrap_or_default()
}

fn clear_detail(widgets: &Widgets, state: &Rc<RefCell<AppState>>) {
    state.borrow_mut().selected_snapshot = None;
    widgets.detail_stack.set_visible_child_name("empty");
    // Restore button has no target without a selection.
    widgets.detail_restore_btn.set_sensitive(false);
}

fn apply_detail(widgets: &Widgets, snap: &Snapshot, strain_subvols: &[String]) {
    widgets.detail_title.set_label(&format_created(snap));
    widgets.detail_subtitle.set_label(&snap.strain);

    match &snap.message {
        Some(m) if !m.is_empty() => {
            // Italic via Pango markup. `markup_escape_text` keeps
            // user-supplied messages safe even though the field is
            // free-form.
            widgets
                .detail_message
                .set_markup(&format!("<i>{}</i>", glib::markup_escape_text(m)));
            widgets.detail_message.set_visible(true);
        }
        _ => {
            widgets.detail_message.set_label("—");
            widgets.detail_message.set_visible(true);
        }
    }

    widgets.detail_pill_anchor.set_visible(snap.is_live_anchor);
    widgets.detail_pill_protected.set_visible(snap.is_protected);

    widgets.detail_trigger.set_label(&snap.trigger);
    widgets
        .detail_subvols
        .set_label(&if strain_subvols.is_empty() {
            "—".to_string()
        } else {
            strain_subvols.join(", ")
        });
    widgets.detail_created.set_label(&format_created_full(snap));

    // Restore button is sensitive whenever a snapshot is shown.
    // While a restore is in flight, the click handler no-ops on
    // `state.restore_in_flight`, and the result handler toggles
    // the sensitivity itself — apply_detail doesn't need to know.
    widgets.detail_restore_btn.set_sensitive(true);

    widgets.detail_stack.set_visible_child_name("populated");
}

/// Render the snapshot's `created` field in long form for the detail
/// pane: full local-time ISO 8601 with offset, falling back to the
/// raw RFC 3339 if it doesn't parse, and to the id if `created` is
/// missing entirely.
fn format_created_full(snap: &Snapshot) -> String {
    match &snap.created {
        Some(rfc) => match chrono::DateTime::parse_from_rfc3339(rfc) {
            Ok(dt) => dt
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string(),
            Err(_) => rfc.clone(),
        },
        None => snap.id.clone(),
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
