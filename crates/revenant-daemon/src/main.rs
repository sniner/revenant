//! `revenantd` — privileged D-Bus daemon for revenant-gui.
//!
//! See `docs/design/dbus-interface.md` for the wire-level contract.

mod config_edit;
mod dbus;
mod errors;
mod marshal;
mod mount;
mod polkit;
mod state;
mod watcher;

use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

const BUS_NAME: &str = "org.revenant.Daemon1";
const OBJECT_PATH: &str = "/org/revenant/Daemon";
const CONFIG_DIR: &str = "/etc/revenant";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    tracing::info!("revenantd {} starting", env!("CARGO_PKG_VERSION"));

    // Initialize state (config + toplevel mount). The state is held by
    // the D-Bus object; dropping the connection drops the object,
    // which drops the state, which umounts the toplevel.
    let state = state::DaemonState::initialize();
    let initial_state_label = match &state.degraded {
        None => ("ready", String::new()),
        Some(reason) => {
            tracing::warn!("daemon running in degraded state: {reason}");
            ("degraded", reason.to_string())
        }
    };
    if initial_state_label.0 == "ready" {
        tracing::info!("daemon ready");
    }

    // Compute watcher paths *before* moving `state` into the Daemon —
    // the snapshot dir lives inside the toplevel mount, the config
    // path is a daemon-wide constant.
    let snapshot_dir = {
        let cfg_guard = state.config.read().await;
        state
            .toplevel
            .as_ref()
            .zip(cfg_guard.as_ref())
            .map(|(mount, cfg)| mount.path().join(&cfg.sys.snapshot_subvol))
    };

    let daemon = dbus::Daemon::new(state);
    let conn = zbus::connection::Builder::system()
        .context("connect to system bus")?
        .name(BUS_NAME)
        .context("request bus name")?
        .serve_at(OBJECT_PATH, daemon)
        .context("export interface")?
        .build()
        .await
        .context("build D-Bus connection")?;

    tracing::info!("registered {BUS_NAME} on {OBJECT_PATH}");

    let object_path =
        zvariant::OwnedObjectPath::try_from(OBJECT_PATH).context("encode object path")?;

    // Announce the current state once via DaemonStateChanged. Clients
    // that subscribed in time get the signal; everyone else uses
    // GetDaemonInfo. The signal is the only mechanism that exists for
    // future state transitions (lazy mount, idle exit), so wiring it
    // now keeps the contract honest even though we don't yet flip
    // state at runtime.
    let (state_label, state_message) = initial_state_label;
    emit_daemon_state_changed(&conn, &object_path, state_label, &state_message).await;

    // Live updates: only run the watchers when we actually have
    // something to watch. A degraded daemon is still usable for
    // metadata calls; it just won't push change notifications.
    if let Some(snap_dir) = snapshot_dir {
        watcher::spawn(
            conn.clone(),
            object_path,
            snap_dir,
            PathBuf::from(CONFIG_DIR),
        );
    } else {
        tracing::warn!("daemon degraded — filesystem watchers not started");
    }

    // Block until SIGINT or SIGTERM. Either way `_conn` then drops,
    // which drops the served `Daemon`, which drops the toplevel mount
    // guard — that's where the umount actually happens.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received"),
        _ = sigterm.recv() => tracing::info!("SIGTERM received"),
    }

    tracing::info!("shutting down");
    Ok(())
}

/// Look up the served Daemon interface and emit `DaemonStateChanged`
/// with the given (state, message). Failure is logged and swallowed
/// so a missing subscriber never aborts startup.
async fn emit_daemon_state_changed(
    conn: &zbus::Connection,
    object_path: &zvariant::OwnedObjectPath,
    state: &str,
    message: &str,
) {
    match conn
        .object_server()
        .interface::<_, dbus::Daemon>(object_path)
        .await
    {
        Ok(iface) => {
            if let Err(e) =
                dbus::Daemon::daemon_state_changed(iface.signal_emitter(), state, message).await
            {
                tracing::warn!("emit DaemonStateChanged({state:?}): {e}");
            }
        }
        Err(e) => tracing::warn!("lookup Daemon iface for DaemonStateChanged: {e}"),
    }
}
