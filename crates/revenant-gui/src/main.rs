//! `revenant-gui` — GTK4/libadwaita frontend for the revenant snapshot tool.
//!
//! Skeleton only: shows an empty `AdwApplicationWindow` with a status
//! placeholder. The real UI (sidebar, snapshot list, detail pane,
//! retention dialog, restore flow) is described in
//! `docs/design/gui-wireframes.md` and will be wired up against the
//! `revenant-daemon` D-Bus service (`docs/design/dbus-interface.md`).

use adw::prelude::*;
use gtk::glib;

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
        .description("GUI not yet implemented")
        .vexpand(true)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content.append(&header);
    content.append(&placeholder);

    window.set_content(Some(&content));
    window.present();
}
