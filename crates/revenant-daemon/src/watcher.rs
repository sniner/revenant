//! Filesystem watchers that turn local changes into D-Bus signals.
//!
//! Two independent watchers run as long-lived tokio tasks:
//!
//! * **Snapshot watcher** on `<toplevel>/<snapshot_subvol>/` — picks up
//!   subvolume creation/deletion and sidecar writes. Emits
//!   `SnapshotsChanged`.
//! * **Config watcher** on `/etc/revenant/` — picks up
//!   user-driven edits to `config.toml`. Emits
//!   `StrainConfigChanged`.
//!
//! `LiveParentChanged` is *not* driven by inotify: there is no
//! filesystem notification for "btrfs parent_uuid changed". The signal
//! is emitted from the restore code path instead (Slice 6).
//!
//! Events are debounced with a short trailing window so a burst of
//! filesystem activity (e.g. a snapshot creation that touches several
//! subvolumes plus a sidecar) collapses into a single signal.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use tokio::time::{Instant, sleep_until};

use crate::dbus::Daemon;

const DEBOUNCE: Duration = Duration::from_millis(200);

const SNAPSHOT_MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::CLOSE_WRITE);

const CONFIG_MASK: WatchMask = WatchMask::CLOSE_WRITE
    .union(WatchMask::CREATE)
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO);

/// Spawn the inotify watchers. Each returns a tokio `JoinHandle` that
/// is currently dropped — the tasks live for the entire daemon
/// runtime. If a watcher fails to start (e.g. directory missing) the
/// failure is logged and the daemon continues; the GUI just won't get
/// live updates for that subtree.
pub fn spawn(
    conn: zbus::Connection,
    object_path: zvariant::OwnedObjectPath,
    snapshot_dir: PathBuf,
    config_dir: PathBuf,
) {
    spawn_one(
        "snapshot watcher",
        snapshot_dir,
        SNAPSHOT_MASK,
        conn.clone(),
        object_path.clone(),
        emit_snapshots_changed,
    );
    spawn_one(
        "config watcher",
        config_dir,
        CONFIG_MASK,
        conn,
        object_path,
        emit_strain_config_changed,
    );
}

fn spawn_one<F>(
    label: &'static str,
    path: PathBuf,
    mask: WatchMask,
    conn: zbus::Connection,
    object_path: zvariant::OwnedObjectPath,
    emit: F,
) where
    F: Fn(zbus::Connection, zvariant::OwnedObjectPath) -> tokio::task::JoinHandle<()>
        + Send
        + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = run_watcher(label, &path, mask, conn, object_path, emit).await {
            tracing::warn!("{label} on {} stopped: {e:#}", path.display());
        }
    });
}

async fn run_watcher<F>(
    label: &'static str,
    path: &Path,
    mask: WatchMask,
    conn: zbus::Connection,
    object_path: zvariant::OwnedObjectPath,
    emit: F,
) -> Result<()>
where
    F: Fn(zbus::Connection, zvariant::OwnedObjectPath) -> tokio::task::JoinHandle<()>,
{
    let inotify = Inotify::init().context("init inotify")?;
    inotify
        .watches()
        .add(path, mask)
        .with_context(|| format!("add watch on {}", path.display()))?;
    tracing::info!("{label} watching {}", path.display());

    let buffer = [0u8; 4096];
    let mut stream = inotify
        .into_event_stream(buffer)
        .context("turn inotify into event stream")?;

    // Trailing-edge debounce: arm a timer on the first event in a
    // quiet period; collapse all further events that arrive before
    // the timer fires into the same emission.
    let mut deadline: Option<Instant> = None;

    loop {
        tokio::select! {
            ev = stream.next() => match ev {
                Some(Ok(_event)) => {
                    deadline.get_or_insert_with(|| Instant::now() + DEBOUNCE);
                }
                Some(Err(e)) => {
                    tracing::warn!("{label}: inotify read error: {e}");
                }
                None => {
                    tracing::warn!("{label}: inotify stream ended");
                    return Ok(());
                }
            },
            _ = wait_until(deadline) => {
                deadline = None;
                emit(conn.clone(), object_path.clone());
            }
        }
    }
}

/// Resolve the `Option<Instant>` deadline into a future that pends
/// forever when there is nothing to wait for, and yields at the
/// instant otherwise.
async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(t) => sleep_until(t).await,
        None => std::future::pending().await,
    }
}

// ---- signal emitters ----------------------------------------------------
//
// Each helper looks up the registered Daemon interface via the
// connection and fires the corresponding signal. We do this in a
// detached task so the watcher loop never blocks on a slow client.

fn emit_snapshots_changed(
    conn: zbus::Connection,
    object_path: zvariant::OwnedObjectPath,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match conn
            .object_server()
            .interface::<_, Daemon>(&object_path)
            .await
        {
            Ok(iface) => {
                if let Err(e) = Daemon::snapshots_changed(iface.signal_emitter(), "").await {
                    tracing::warn!("emit SnapshotsChanged: {e}");
                }
            }
            Err(e) => tracing::warn!("lookup Daemon iface for SnapshotsChanged: {e}"),
        }
    })
}

fn emit_strain_config_changed(
    conn: zbus::Connection,
    object_path: zvariant::OwnedObjectPath,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match conn
            .object_server()
            .interface::<_, Daemon>(&object_path)
            .await
        {
            Ok(iface) => {
                if let Err(e) = Daemon::strain_config_changed(iface.signal_emitter()).await {
                    tracing::warn!("emit StrainConfigChanged: {e}");
                }
            }
            Err(e) => tracing::warn!("lookup Daemon iface for StrainConfigChanged: {e}"),
        }
    })
}
