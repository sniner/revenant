//! Read-only snapshot mounts: result handlers and file-manager launch.
//!
//! The user-visible flow:
//!   1. user clicks the toggle → row dispatches `MountSnapshot`,
//!      disables the toggle while the call is in flight (the toggle's
//!      visual state has already flipped optimistically).
//!   2. daemon mounts every subvolume of the snapshot read-only and
//!      returns a `subvol_name -> mount_path` map.
//!   3. on success we launch the user's default file manager on the
//!      common parent of those paths so they land directly inside the
//!      snapshot. On failure we surface a toast and reload the strain
//!      list so the toggle reverts to its previous visual state.
//!
//! `UnmountSnapshot` follows the same shape, minus the launch step.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use gtk::gio;

use crate::dbus_thread::Command;
use crate::ui::format::show_error_toast;
use crate::{AppState, Widgets};

pub(crate) fn apply_mount_snapshot_result(
    parent: &adw::ApplicationWindow,
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    strain: &str,
    id: &str,
    result: Result<HashMap<String, String>, String>,
) {
    let key = (strain.to_string(), id.to_string());
    let mut needs_reload = false;
    {
        let mut st = state.borrow_mut();
        st.mount_in_flight.remove(&key);
        match &result {
            Ok(paths) if !paths.is_empty() => {
                st.mounted_snapshots.insert(key.clone());
            }
            _ => {
                // Either a hard failure or an empty paths map. Either
                // way the optimistic toggle flip is wrong; the reload
                // below rebuilds the row from `mounted_snapshots` and
                // reverts.
                needs_reload = true;
            }
        }
    }

    match result {
        Ok(paths) if paths.is_empty() => {
            tracing::warn!("MountSnapshot({strain}@{id}) returned an empty path map");
            show_error_toast(
                &widgets.toast_overlay,
                "Mount returned nothing",
                "Daemon reported no subvolumes for this snapshot.",
            );
        }
        Ok(paths) => {
            // Every value lives under the snapshot's per-uid base dir;
            // `<path>/..` is that base dir. Picking any value is fine.
            if let Some(target) = paths
                .values()
                .next()
                .and_then(|p| Path::new(p).parent().map(Path::to_path_buf))
            {
                launch_file_manager(parent, &target);
            }
        }
        Err(reason) => {
            tracing::warn!("MountSnapshot({strain}@{id}) failed: {reason}");
            widgets
                .toast_overlay
                .add_toast(adw::Toast::new(&format!("Mount failed: {reason}")));
        }
    }

    if needs_reload {
        reload_current_strain_if(state, cmd_tx, strain);
    } else {
        // Even on success the toggle currently shows
        // `sensitive=false`. Cheapest way to flip it back to
        // sensitive without bookkeeping per-row widget refs is to
        // re-render the strain list from cached state.
        reload_current_strain_if(state, cmd_tx, strain);
    }
}

pub(crate) fn apply_unmount_snapshot_result(
    widgets: &Widgets,
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    strain: &str,
    id: &str,
    result: Result<(), String>,
) {
    let key = (strain.to_string(), id.to_string());
    {
        let mut st = state.borrow_mut();
        st.mount_in_flight.remove(&key);
        if result.is_ok() {
            st.mounted_snapshots.remove(&key);
        }
        // On failure we keep the entry in `mounted_snapshots` so the
        // toggle reverts to its mounted state on the upcoming reload.
    }

    if let Err(reason) = &result {
        tracing::warn!("UnmountSnapshot({strain}@{id}) failed: {reason}");
        widgets
            .toast_overlay
            .add_toast(adw::Toast::new(&format!("Unmount failed: {reason}")));
    }

    reload_current_strain_if(state, cmd_tx, strain);
}

/// Trigger a fresh `LoadSnapshots(strain)` only if `strain` is the
/// one currently selected in the sidebar — otherwise the user has
/// moved on and the row no longer exists.
fn reload_current_strain_if(
    state: &Rc<RefCell<AppState>>,
    cmd_tx: &async_channel::Sender<Command>,
    strain: &str,
) {
    let selected = state.borrow().selected_strain.clone();
    if selected.as_deref() == Some(strain) {
        let _ = cmd_tx.send_blocking(Command::LoadSnapshots(strain.to_string()));
    }
}

/// Launch the user's default handler on `path` (a directory). The
/// callback is fire-and-forget; failures end up as a tracing warning
/// so an unconfigured DE doesn't go silent on the user.
fn launch_file_manager(parent: &adw::ApplicationWindow, path: &Path) {
    let file = gio::File::for_path(path);
    let launcher = gtk::FileLauncher::new(Some(&file));
    let path_for_log = path.to_path_buf();
    launcher.launch(Some(parent), gio::Cancellable::NONE, move |res| {
        if let Err(e) = res {
            tracing::warn!(
                "file-manager launch on {} failed: {e}",
                path_for_log.display(),
            );
        }
    });
}
