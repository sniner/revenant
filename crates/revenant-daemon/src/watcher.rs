//! Filesystem watchers that turn local changes into D-Bus signals.
//!
//! Two independent watchers run as long-lived tokio tasks:
//!
//! * **Snapshot watcher** on `<toplevel>/<snapshot_subvol>/` — picks up
//!   subvolume creation/deletion and sidecar writes. Emits
//!   `SnapshotsChanged(strain)`, where `strain` is derived from the
//!   touched filename. If a debounce window contains an event whose
//!   strain cannot be parsed, a single `SnapshotsChanged("")` ("any")
//!   is emitted instead so subscribers play it safe and refresh
//!   everything.
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
//! subvolumes plus a sidecar) collapses into a single signal per
//! strain.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use revenant_core::metadata::parse_sidecar_name;
use revenant_core::snapshot::parse_snapshot_subvol_name;
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

/// Spawn the inotify watchers. Each tokio task lives for the entire
/// daemon runtime. If a watcher fails to start (e.g. directory
/// missing) the failure is logged and the daemon continues; the GUI
/// just won't get live updates for that subtree.
pub fn spawn(
    conn: zbus::Connection,
    object_path: zvariant::OwnedObjectPath,
    snapshot_dir: PathBuf,
    config_dir: PathBuf,
) {
    {
        let conn = conn.clone();
        let object_path = object_path.clone();
        tokio::spawn(async move {
            if let Err(e) = run_snapshot_watcher(&snapshot_dir, conn, object_path).await {
                tracing::warn!(
                    "snapshot watcher on {} stopped: {e:#}",
                    snapshot_dir.display()
                );
            }
        });
    }
    tokio::spawn(async move {
        if let Err(e) = run_config_watcher(&config_dir, conn, object_path).await {
            tracing::warn!("config watcher on {} stopped: {e:#}", config_dir.display());
        }
    });
}

/// Snapshot-directory watcher. Tracks the set of strains touched
/// during the current debounce window plus an "unknown" flag for
/// names that don't parse as either a snapshot subvolume or a sidecar.
async fn run_snapshot_watcher(
    path: &Path,
    conn: zbus::Connection,
    object_path: zvariant::OwnedObjectPath,
) -> Result<()> {
    let mut stream = open_stream("snapshot watcher", path, SNAPSHOT_MASK)?;

    let mut deadline: Option<Instant> = None;
    let mut strains: HashSet<String> = HashSet::new();
    let mut any_unknown = false;

    loop {
        tokio::select! {
            ev = stream.next() => match ev {
                Some(Ok(event)) => {
                    match classify_event(event.name.as_deref()) {
                        Some(strain) => { strains.insert(strain); }
                        None => { any_unknown = true; }
                    }
                    deadline.get_or_insert_with(|| Instant::now() + DEBOUNCE);
                }
                Some(Err(e)) => {
                    tracing::warn!("snapshot watcher: inotify read error: {e}");
                }
                None => {
                    tracing::warn!("snapshot watcher: inotify stream ended");
                    return Ok(());
                }
            },
            _ = wait_until(deadline) => {
                deadline = None;
                let to_emit: Vec<String> = if any_unknown {
                    // Conservative fallback: one "any" signal covers
                    // everything, including any per-strain hits we
                    // also collected.
                    vec![String::new()]
                } else {
                    strains.iter().cloned().collect()
                };
                strains.clear();
                any_unknown = false;
                for strain in to_emit {
                    spawn_emit_snapshots_changed(conn.clone(), object_path.clone(), strain);
                }
            }
        }
    }
}

/// Config-directory watcher. Single-shot debounced emission per window.
async fn run_config_watcher(
    path: &Path,
    conn: zbus::Connection,
    object_path: zvariant::OwnedObjectPath,
) -> Result<()> {
    let mut stream = open_stream("config watcher", path, CONFIG_MASK)?;

    let mut deadline: Option<Instant> = None;

    loop {
        tokio::select! {
            ev = stream.next() => match ev {
                Some(Ok(_event)) => {
                    deadline.get_or_insert_with(|| Instant::now() + DEBOUNCE);
                }
                Some(Err(e)) => {
                    tracing::warn!("config watcher: inotify read error: {e}");
                }
                None => {
                    tracing::warn!("config watcher: inotify stream ended");
                    return Ok(());
                }
            },
            _ = wait_until(deadline) => {
                deadline = None;
                spawn_emit_strain_config_changed(conn.clone(), object_path.clone());
            }
        }
    }
}

/// Initialise an inotify watch on `path` and return its event stream.
fn open_stream(
    label: &'static str,
    path: &Path,
    mask: WatchMask,
) -> Result<inotify::EventStream<[u8; 4096]>> {
    let inotify = Inotify::init().context("init inotify")?;
    inotify
        .watches()
        .add(path, mask)
        .with_context(|| format!("add watch on {}", path.display()))?;
    tracing::info!("{label} watching {}", path.display());

    let buffer = [0u8; 4096];
    inotify
        .into_event_stream(buffer)
        .context("turn inotify into event stream")
}

/// Map an inotify event name to a strain, if it parses as a snapshot
/// subvolume or a sidecar file. Returns `None` for events on the
/// directory itself, anonymous events, or names that don't fit either
/// shape — those force a fallback "any" emission. The unparseable case
/// is logged at debug level so its frequency can be inspected without
/// turning info-level traffic into noise.
fn classify_event(name: Option<&OsStr>) -> Option<String> {
    let raw = name?.to_str()?;
    if let Some((strain, _id)) = parse_sidecar_name(raw) {
        return Some(strain);
    }
    if let Some((_subvol, strain, _id)) = parse_snapshot_subvol_name(raw) {
        return Some(strain);
    }
    tracing::debug!(name = %raw, "watcher: event name did not parse as snapshot or sidecar");
    None
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

fn spawn_emit_snapshots_changed(
    conn: zbus::Connection,
    object_path: zvariant::OwnedObjectPath,
    strain: String,
) {
    tokio::spawn(async move {
        match conn
            .object_server()
            .interface::<_, Daemon>(&object_path)
            .await
        {
            Ok(iface) => {
                if let Err(e) = Daemon::snapshots_changed(iface.signal_emitter(), &strain).await {
                    tracing::warn!("emit SnapshotsChanged({strain:?}): {e}");
                }
            }
            Err(e) => tracing::warn!("lookup Daemon iface for SnapshotsChanged: {e}"),
        }
    });
}

fn spawn_emit_strain_config_changed(
    conn: zbus::Connection,
    object_path: zvariant::OwnedObjectPath,
) {
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
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_subvol_event() {
        assert_eq!(
            classify_event(Some(OsStr::new("@-default-20260316-143022-456"))),
            Some("default".to_string())
        );
        assert_eq!(
            classify_event(Some(OsStr::new("@home-periodic-20260316-143022"))),
            Some("periodic".to_string())
        );
    }

    #[test]
    fn classify_sidecar_event() {
        assert_eq!(
            classify_event(Some(OsStr::new("default-20260316-143022-456.meta.toml"))),
            Some("default".to_string())
        );
    }

    #[test]
    fn classify_unknown_event() {
        assert_eq!(classify_event(None), None);
        assert_eq!(classify_event(Some(OsStr::new("random"))), None);
        assert_eq!(classify_event(Some(OsStr::new("foo.tmp"))), None);
    }
}
