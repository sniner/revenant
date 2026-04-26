//! `revenant-gui` — GTK4/libadwaita frontend for the revenant snapshot tool.
//!
//! Slice state: 4a — D-Bus client wired up. The worker thread
//! connects to `org.revenant.Daemon1` on the system bus, fetches
//! `GetDaemonInfo`, and the placeholder reflects the result. Sidebar,
//! snapshot list, detail pane and the rest of the wireframes
//! (`docs/design/gui-wireframes.md`) come in subsequent slices.

mod client;
mod dbus_thread;
mod proxy;

use adw::prelude::*;
use gtk::glib;

use crate::dbus_thread::Event;
use crate::proxy::Dict;

const APP_ID: &str = "org.revenant.Gui";

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

    let placeholder = adw::StatusPage::builder()
        .icon_name("drive-harddisk-symbolic")
        .title("Revenant")
        .description("Connecting to revenantd…")
        .vexpand(true)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content.append(&header);
    content.append(&placeholder);

    window.set_content(Some(&content));
    window.present();

    // Worker thread runs the zbus client on its own tokio runtime;
    // the receiver is drained on the glib MainContext and updates
    // the placeholder text from there. Cloning the StatusPage just
    // bumps a refcount — both copies refer to the same widget.
    let events = dbus_thread::spawn();
    let placeholder_for_events = placeholder.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = events.recv().await {
            apply_event(&placeholder_for_events, event);
        }
    });
}

fn apply_event(placeholder: &adw::StatusPage, event: Event) {
    match event {
        Event::Connected => {
            tracing::info!("daemon connected");
            placeholder.set_description(Some("Loading daemon info…"));
        }
        Event::Disconnected(reason) => {
            tracing::warn!("daemon connect failed: {reason}");
            placeholder.set_description(Some(&format!("Daemon unavailable: {reason}")));
        }
        Event::DaemonInfo(Ok(info)) => {
            placeholder.set_description(Some(&summarize_info(&info)));
        }
        Event::DaemonInfo(Err(reason)) => {
            tracing::warn!("GetDaemonInfo failed: {reason}");
            placeholder.set_description(Some(&format!("Daemon error: {reason}")));
        }
    }
}

/// Compact one-line summary of the daemon-info dict. Slice 4b will
/// surface this in the proper live-state footer; for now it just
/// proves end-to-end connectivity in the placeholder.
fn summarize_info(info: &Dict) -> String {
    let version = read_str(info, "version").unwrap_or("?");
    let backend = read_str(info, "backend").unwrap_or("unknown");
    let mounted = read_bool(info, "toplevel_mounted").unwrap_or(false);
    let state = if mounted { "ready" } else { "degraded" };
    match read_str(info, "degraded_reason") {
        Some(reason) => format!("revenantd {version} • backend={backend} • {state} ({reason})"),
        None => format!("revenantd {version} • backend={backend} • {state}"),
    }
}

fn read_str<'a>(dict: &'a Dict, key: &str) -> Option<&'a str> {
    dict.get(key).and_then(|v| <&str>::try_from(v).ok())
}

fn read_bool(dict: &Dict, key: &str) -> Option<bool> {
    dict.get(key).and_then(|v| bool::try_from(v).ok())
}
