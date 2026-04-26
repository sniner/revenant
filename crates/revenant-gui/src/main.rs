//! `revenant-gui` — GTK4/libadwaita frontend for the revenant snapshot tool.
//!
//! Slice state: 4b — strain sidebar, flat snapshot list, live-state
//! footer. Detail pane (4c), retention editor (4d), restore flow
//! (4e) and live signal subscriptions (4f) come in subsequent
//! slices. Layout follows `docs/design/gui-wireframes.md`.

mod client;
mod dbus_thread;
mod model;
mod proxy;

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::dbus_thread::{Command, Event, Handles};
use crate::model::{LiveParent, Snapshot, Strain};
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
    let widgets = Widgets {
        root_stack: root_stack.clone(),
        status_page: status_page.clone(),
        ..widgets
    };

    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    outer.append(&header);
    outer.append(&root_stack);
    window.set_content(Some(&outer));
    window.present();

    let handles = dbus_thread::spawn();
    wire_event_loop(widgets, handles);
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

    // ---- content -----------------------------------------------------

    let content_title = gtk::Label::builder()
        .label("")
        .xalign(0.0)
        .css_classes(["title-2"])
        .margin_top(18)
        .margin_bottom(12)
        .margin_start(18)
        .margin_end(18)
        .build();

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
        .description("This strain has no snapshots yet.")
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
    content.append(&content_title);
    content.append(&snapshot_stack);

    // ---- assemble ----------------------------------------------------

    let split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar)
        .content(&content)
        .min_sidebar_width(220.0)
        .max_sidebar_width(320.0)
        .build();
    root_stack.add_named(&split, Some("main"));

    Widgets {
        root_stack: root_stack.clone(),
        status_page: adw::StatusPage::new(), // placeholder, overwritten by caller
        strain_list,
        snapshot_stack,
        snapshot_list,
        snapshot_empty,
        snapshot_error,
        content_title,
        footer_state,
        footer_live,
        footer_live_detail,
    }
}

fn wire_event_loop(widgets: Widgets, handles: Handles) {
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

    state.borrow_mut().strains = strains.clone();

    if let Some(first) = strains.first() {
        // Auto-select the first strain so the user lands on something
        // immediately. `select_row` re-emits row-selected, which sends
        // the LoadSnapshots command; no need to dispatch here.
        if let Some(row) = widgets.strain_list.row_at_index(0) {
            widgets.strain_list.select_row(Some(&row));
        }
        // Defensive: if select_row didn't fire (already selected), make
        // sure we still kick off the initial snapshot fetch.
        if state.borrow().selected_strain.as_deref() != Some(first.name.as_str()) {
            select_strain(state, widgets, &first.name);
            let _ = cmd_tx.send_blocking(Command::LoadSnapshots(first.name.clone()));
        }
    } else {
        widgets.content_title.set_label("");
        widgets
            .snapshot_empty
            .set_description(Some("No strains configured."));
        widgets.snapshot_stack.set_visible_child_name("empty");
    }
}

fn select_strain(state: &Rc<RefCell<AppState>>, widgets: &Widgets, name: &str) {
    state.borrow_mut().selected_strain = Some(name.to_string());
    widgets.content_title.set_label(name);
    widgets.snapshot_stack.set_visible_child_name("loading");
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
            widgets
                .snapshot_empty
                .set_description(Some("This strain has no snapshots yet."));
            widgets.snapshot_stack.set_visible_child_name("empty");
        }
        Ok(snaps) => {
            // Daemon sorts oldest-first by id; reverse for newest-on-top
            // display matching the wireframes.
            for snap in snaps.into_iter().rev() {
                widgets.snapshot_list.append(&snapshot_row(&snap));
            }
            widgets.snapshot_stack.set_visible_child_name("list");
        }
        Err(reason) => {
            tracing::warn!("ListSnapshots({strain}) failed: {reason}");
            widgets.snapshot_error.set_description(Some(&reason));
            widgets.snapshot_stack.set_visible_child_name("error");
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
