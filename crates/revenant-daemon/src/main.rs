//! `revenantd` — privileged D-Bus daemon for revenant-gui.
//!
//! See `docs/design/dbus-interface.md` for the wire-level contract.

mod dbus;
mod marshal;
mod mount;
mod state;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

const BUS_NAME: &str = "org.revenant.Daemon1";
const OBJECT_PATH: &str = "/org/revenant/Daemon";

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
    if let Some(reason) = &state.degraded {
        tracing::warn!("daemon running in degraded state: {reason}");
    } else {
        tracing::info!("daemon ready");
    }

    let daemon = dbus::Daemon::new(state);
    let _conn = zbus::connection::Builder::system()
        .context("connect to system bus")?
        .name(BUS_NAME)
        .context("request bus name")?
        .serve_at(OBJECT_PATH, daemon)
        .context("export interface")?
        .build()
        .await
        .context("build D-Bus connection")?;

    tracing::info!("registered {BUS_NAME} on {OBJECT_PATH}");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received"),
        _ = sigterm.recv() => tracing::info!("SIGTERM received"),
    }

    tracing::info!("shutting down");
    Ok(())
}
