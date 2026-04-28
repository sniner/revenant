//! Bridge between the GTK4 main loop (glib `MainContext`) and the
//! tokio runtime that drives the zbus client.
//!
//! Architecture: a dedicated OS thread owns a current-thread tokio
//! runtime. The runtime hosts the daemon `Client` and runs an event
//! loop that connects, fetches initial state, and pushes events back
//! across an `async-channel`. The GTK side `await`s receiver
//! events on the glib executor and updates widgets. A second channel
//! carries commands GUI → worker (e.g. "load snapshots for strain X").
//!
//! Why a worker thread: zbus is built on tokio; glib has its own
//! executor. Polling tokio futures from glib (or vice-versa) is
//! brittle. Two single-threaded runtimes connected by a runtime-
//! agnostic channel is the predictable shape.
//!
//! Reconnect scope for slice 4b: retry the initial bus connect with
//! a fixed 5 s backoff. Once connected, the worker pushes the
//! initial fetches and then services commands. Detecting daemon
//! death mid-session and rebuilding the proxy is deferred — D-Bus
//! activation re-spawns revenantd on the next method call, and the
//! existing zbus connection survives across that.
//!
//! Signal subscriptions (slice 4f): once connected, the worker
//! spawns one tokio task per signal stream from the daemon. Each
//! task pushes follow-up Events back across the same channel, so
//! the GUI side reacts to live updates the same way it reacts to
//! initial fetches — no separate code path. The streams stay
//! alive for the lifetime of the runtime; dropping the worker
//! thread cleans everything up.

use std::collections::HashMap;
use std::time::Duration;

use async_channel::{Receiver, Sender, bounded, unbounded};
use futures_util::StreamExt;

use crate::client::Client;
use crate::model::{DeleteMarker, LiveParent, RestoreOutcome, Retention, Snapshot, Strain};
use crate::proxy::{DaemonProxy, Dict};

/// Events pushed from the worker to the GUI thread.
#[derive(Debug)]
pub enum Event {
    /// Worker has a live `Client` against the system bus.
    Connected,
    /// Connect attempt failed; the worker will keep retrying.
    /// Carries the human-readable reason for the latest attempt.
    Disconnected(String),
    /// Result of `GetDaemonInfo` (initial call or refresh after a
    /// `DaemonStateChanged` signal).
    DaemonInfo(Result<Dict, String>),
    /// Result of `ListStrains` (initial call or refresh after a
    /// `StrainConfigChanged` signal).
    Strains(Result<Vec<Strain>, String>),
    /// Result of `GetLatestStrain` — name of the strain that holds the
    /// most recently created snapshot, or `""` for "no preference".
    /// Sent once at startup so the GUI can pick a sensible initial
    /// selection. Emitted *before* the corresponding `Strains` event.
    LatestStrain(String),
    /// Result of `GetLiveParent` (initial call or refresh after a
    /// `LiveParentChanged` signal). `Ok(None)` is the empty-dict
    /// sentinel ("pristine system / anchor lost").
    LiveParent(Result<Option<LiveParent>, String>),
    /// Result of `ListSnapshots(strain)`. The strain is echoed back
    /// so the GUI can route the result to the right list even if the
    /// user has moved on in the meantime.
    Snapshots {
        strain: String,
        result: Result<Vec<Snapshot>, String>,
    },
    /// Daemon emitted `SnapshotsChanged(strain)`. Empty `strain`
    /// means "any/all". Only the GUI knows which strain is currently
    /// shown, so the worker forwards the signal verbatim and lets
    /// the GUI decide whether to issue a follow-up `LoadSnapshots`.
    SignalSnapshotsChanged(String),
    /// Result of a privileged `Restore` call. Polkit prompt happens
    /// inside the daemon; this fires once the user has responded to
    /// it and the restore (or dry-run) is finished. `Err` covers
    /// polkit-cancel, preflight-blocked, and generic failures alike.
    RestoreResult {
        /// Echo of the request — the GUI uses this to confirm the
        /// banner refers to the snapshot the user actually clicked,
        /// not a stale earlier request.
        strain: String,
        id: String,
        result: Result<RestoreOutcome, String>,
    },
    /// Result of a `SetStrainRetention` call. Echoes the strain so
    /// the dialog's response handler can match the result up to the
    /// dialog instance that issued it.
    RetentionResult {
        strain: String,
        result: Result<(), String>,
    },
    /// Result of a privileged `CreateSnapshot` call. On success
    /// carries the freshly-created snapshot's typed form so the GUI
    /// can name it in the success toast without re-fetching. The
    /// list itself updates via `SnapshotsChanged` (slice 4f).
    CreateSnapshotResult {
        strain: String,
        result: Result<Snapshot, String>,
    },
    /// Result of a privileged `DeleteSnapshot` call. The strain/id
    /// pair is echoed so the toast handler can name the deleted
    /// snapshot without holding extra state. The list refreshes via
    /// `SnapshotsChanged` on success.
    DeleteSnapshotResult {
        strain: String,
        id: String,
        result: Result<(), String>,
    },
    /// Result of `ListDeleteMarkers` — the current set of pre-restore
    /// states. Sent on initial fetch and after every
    /// `DeleteMarkersChanged` signal. The header-button visibility and
    /// the review dialog both read this.
    DeleteMarkers(Result<Vec<DeleteMarker>, String>),
    /// Result of a privileged `PurgeDeleteMarkers` call. Carries the
    /// list of marker names actually removed (may be a strict subset of
    /// the requested names if a concurrent CLI cleanup beat us to some).
    PurgeDeleteMarkersResult(Result<Vec<String>, String>),
}

/// Commands sent from the GUI to the worker.
#[derive(Debug)]
pub enum Command {
    /// Fetch the snapshot list for a strain. Worker replies with
    /// `Event::Snapshots`.
    LoadSnapshots(String),
    /// Issue a privileged `Restore` call. Worker replies with
    /// `Event::RestoreResult`. The polkit prompt is the daemon's
    /// problem; this future just awaits its outcome.
    Restore {
        strain: String,
        id: String,
        save_current: bool,
        dry_run: bool,
    },
    /// Update a strain's retention policy. Worker replies with
    /// `Event::RetentionResult`. The daemon emits
    /// `StrainConfigChanged` on success, so 4f's signal handler
    /// re-fetches the strain list automatically.
    SetRetention {
        strain: String,
        retention: Retention,
    },
    /// Issue a privileged `CreateSnapshot` call. Worker replies with
    /// `Event::CreateSnapshotResult`. Polkit prompt happens inside
    /// the daemon. `message` is the metadata sidecar's message vector;
    /// pass an empty vec to omit it.
    CreateSnapshot {
        strain: String,
        message: Vec<String>,
    },
    /// Issue a privileged `DeleteSnapshot` call. Worker replies with
    /// `Event::DeleteSnapshotResult`.
    DeleteSnapshot { strain: String, id: String },
    /// Purge the named DELETE markers. Worker replies with
    /// `Event::PurgeDeleteMarkersResult`. Polkit prompt happens
    /// inside the daemon.
    PurgeDeleteMarkers(Vec<String>),
}

/// Worker handle returned to the GUI thread.
pub struct Handles {
    pub events: Receiver<Event>,
    pub commands: Sender<Command>,
}

/// Spawn the worker thread. Returns the event receiver and command
/// sender. Dropping either does not stop the worker — it'll just
/// block forever on the next send/recv. For the slice-4b lifecycle
/// (worker dies with the process) that's fine.
pub fn spawn() -> Handles {
    let (evt_tx, evt_rx) = bounded::<Event>(32);
    let (cmd_tx, cmd_rx) = unbounded::<Command>();

    std::thread::Builder::new()
        .name("revenant-dbus".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread tokio runtime");
            rt.block_on(run_loop(evt_tx, cmd_rx));
        })
        .expect("spawn revenant-dbus thread");

    Handles {
        events: evt_rx,
        commands: cmd_tx,
    }
}

async fn run_loop(evt_tx: Sender<Event>, cmd_rx: Receiver<Command>) {
    let client = loop {
        match Client::connect().await {
            Ok(c) => break c,
            Err(e) => {
                let _ = evt_tx.send(Event::Disconnected(format!("{e:#}"))).await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    };
    let _ = evt_tx.send(Event::Connected).await;

    // Spawn the signal-subscription tasks BEFORE the initial fetches.
    // zbus's MatchRule for each signal is added when the stream is
    // first awaited; emitting initial-state events afterwards avoids
    // a (small) window where a SnapshotsChanged from a concurrent
    // CLI snapshot creation would slip past us. The tasks then keep
    // the GUI in sync for the rest of the session.
    spawn_signal_listeners(client.proxy(), evt_tx.clone()).await;

    fetch_initial(&client, &evt_tx).await;

    while let Ok(cmd) = cmd_rx.recv().await {
        handle_command(&client, &evt_tx, cmd).await;
    }
}

/// Subscribe to all four daemon signals and spawn one tokio task
/// per stream. Each task forwards into the same `Event` channel the
/// GUI already drains, so the live-update path reuses the existing
/// rendering code paths. The `proxy` is cloned per task because
/// zbus's signal-driven re-fetches need their own handle.
async fn spawn_signal_listeners(proxy: &DaemonProxy<'static>, evt_tx: Sender<Event>) {
    // SnapshotsChanged → forwarded as-is; the GUI knows which strain
    // is currently shown.
    match proxy.receive_snapshots_changed().await {
        Ok(mut stream) => {
            let evt_tx = evt_tx.clone();
            tokio::spawn(async move {
                while let Some(sig) = stream.next().await {
                    let strain = sig.args().map(|a| a.strain).unwrap_or_default();
                    if evt_tx
                        .send(Event::SignalSnapshotsChanged(strain))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        Err(e) => tracing::warn!("subscribe SnapshotsChanged: {e}"),
    }

    // LiveParentChanged → re-fetch GetLiveParent and emit the same
    // event the initial fetch produces; GUI's existing handler does
    // the rest.
    match proxy.receive_live_parent_changed().await {
        Ok(mut stream) => {
            let evt_tx = evt_tx.clone();
            let proxy = proxy.clone();
            tokio::spawn(async move {
                while stream.next().await.is_some() {
                    let live = proxy
                        .get_live_parent()
                        .await
                        .map(|d| LiveParent::from_dict(&d))
                        .map_err(|e| format!("{e}"));
                    if evt_tx.send(Event::LiveParent(live)).await.is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => tracing::warn!("subscribe LiveParentChanged: {e}"),
    }

    // StrainConfigChanged → re-fetch ListStrains.
    match proxy.receive_strain_config_changed().await {
        Ok(mut stream) => {
            let evt_tx = evt_tx.clone();
            let proxy = proxy.clone();
            tokio::spawn(async move {
                while stream.next().await.is_some() {
                    let strains = proxy
                        .list_strains()
                        .await
                        .map(|v| v.into_iter().map(Strain::from_tuple).collect())
                        .map_err(|e| format!("{e}"));
                    if evt_tx.send(Event::Strains(strains)).await.is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => tracing::warn!("subscribe StrainConfigChanged: {e}"),
    }

    // DaemonStateChanged → re-fetch GetDaemonInfo. The signal
    // payload is a (state, message) string pair; we ignore it and
    // resync the full info dict so the footer stays consistent with
    // any other key (version, backend, toplevel_mounted).
    match proxy.receive_daemon_state_changed().await {
        Ok(mut stream) => {
            let evt_tx = evt_tx.clone();
            let proxy = proxy.clone();
            tokio::spawn(async move {
                while stream.next().await.is_some() {
                    let info = proxy.get_daemon_info().await.map_err(|e| format!("{e}"));
                    if evt_tx.send(Event::DaemonInfo(info)).await.is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => tracing::warn!("subscribe DaemonStateChanged: {e}"),
    }

    // DeleteMarkersChanged → re-fetch ListDeleteMarkers. Drives the
    // header-button visibility and refreshes any open review dialog.
    match proxy.receive_delete_markers_changed().await {
        Ok(mut stream) => {
            let evt_tx = evt_tx.clone();
            let proxy = proxy.clone();
            tokio::spawn(async move {
                while stream.next().await.is_some() {
                    let res = proxy
                        .list_delete_markers()
                        .await
                        .map(|raw| raw.iter().filter_map(DeleteMarker::from_dict).collect())
                        .map_err(|e| format!("{e}"));
                    if evt_tx.send(Event::DeleteMarkers(res)).await.is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => tracing::warn!("subscribe DeleteMarkersChanged: {e}"),
    }
}

async fn fetch_initial(client: &Client, evt_tx: &Sender<Event>) {
    let info = client
        .proxy()
        .get_daemon_info()
        .await
        .map_err(|e| format!("{e}"));
    let _ = evt_tx.send(Event::DaemonInfo(info)).await;

    // Latest-strain hint goes out *before* the strain list so the
    // sidebar handler can use it the first time it runs without
    // racing a follow-up event. Errors are swallowed (we just lose
    // the hint and fall back to first-alphabetical).
    if let Ok(latest) = client.proxy().get_latest_strain().await {
        let _ = evt_tx.send(Event::LatestStrain(latest)).await;
    }

    let strains = client
        .proxy()
        .list_strains()
        .await
        .map(|v| v.into_iter().map(Strain::from_tuple).collect())
        .map_err(|e| format!("{e}"));
    let _ = evt_tx.send(Event::Strains(strains)).await;

    let live = client
        .proxy()
        .get_live_parent()
        .await
        .map(|d| LiveParent::from_dict(&d))
        .map_err(|e| format!("{e}"));
    let _ = evt_tx.send(Event::LiveParent(live)).await;

    // Pre-restore states drive the header-bar notification button.
    // Empty list at startup is the common case; the header just hides
    // the button. Errors fall through to a logged warning at the GUI
    // side and the same hidden state.
    let markers = client
        .proxy()
        .list_delete_markers()
        .await
        .map(|raw| raw.iter().filter_map(DeleteMarker::from_dict).collect())
        .map_err(|e| format!("{e}"));
    let _ = evt_tx.send(Event::DeleteMarkers(markers)).await;
}

async fn handle_command(client: &Client, evt_tx: &Sender<Event>, cmd: Command) {
    match cmd {
        Command::LoadSnapshots(strain) => {
            let result = list_snapshots_for(client, &strain).await;
            let _ = evt_tx.send(Event::Snapshots { strain, result }).await;
        }
        Command::Restore {
            strain,
            id,
            save_current,
            dry_run,
        } => {
            let result = restore(client, &strain, &id, save_current, dry_run).await;
            let _ = evt_tx
                .send(Event::RestoreResult { strain, id, result })
                .await;
        }
        Command::SetRetention { strain, retention } => {
            let result = set_retention(client, &strain, retention).await;
            let _ = evt_tx.send(Event::RetentionResult { strain, result }).await;
        }
        Command::CreateSnapshot { strain, message } => {
            let result = create_snapshot(client, &strain, message).await;
            let _ = evt_tx
                .send(Event::CreateSnapshotResult { strain, result })
                .await;
        }
        Command::DeleteSnapshot { strain, id } => {
            let result = client
                .proxy()
                .delete_snapshot(&strain, &id)
                .await
                .map_err(|e| format!("{e}"));
            let _ = evt_tx
                .send(Event::DeleteSnapshotResult { strain, id, result })
                .await;
        }
        Command::PurgeDeleteMarkers(names) => {
            let result = client
                .proxy()
                .purge_delete_markers(names)
                .await
                .map_err(|e| format!("{e}"));
            let _ = evt_tx.send(Event::PurgeDeleteMarkersResult(result)).await;
        }
    }
}

async fn create_snapshot(
    client: &Client,
    strain: &str,
    message: Vec<String>,
) -> Result<Snapshot, String> {
    let raw = client
        .proxy()
        .create_snapshot(strain, message)
        .await
        .map_err(|e| format!("{e}"))?;
    Snapshot::from_dict(&raw)
        .ok_or_else(|| "daemon returned malformed CreateSnapshot result dict".to_string())
}

async fn list_snapshots_for(client: &Client, strain: &str) -> Result<Vec<Snapshot>, String> {
    let mut filter: Dict = HashMap::new();
    filter.insert(
        "strain".to_string(),
        zvariant::Value::new(strain)
            .try_to_owned()
            .map_err(|e| format!("encode filter: {e}"))?,
    );
    let raw = client
        .proxy()
        .list_snapshots(filter)
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(raw.iter().filter_map(Snapshot::from_dict).collect())
}

async fn set_retention(client: &Client, strain: &str, retention: Retention) -> Result<(), String> {
    let mut dict: Dict = HashMap::new();
    let mut put_u32 = |k: &str, v: u32| -> Result<(), String> {
        let owned = zvariant::Value::new(v)
            .try_to_owned()
            .map_err(|e| format!("encode tier {k}: {e}"))?;
        dict.insert(k.to_string(), owned);
        Ok(())
    };
    put_u32("last", retention.last)?;
    put_u32("hourly", retention.hourly)?;
    put_u32("daily", retention.daily)?;
    put_u32("weekly", retention.weekly)?;
    put_u32("monthly", retention.monthly)?;
    put_u32("yearly", retention.yearly)?;

    client
        .proxy()
        .set_strain_retention(strain, dict)
        .await
        .map_err(|e| format!("{e}"))
}

async fn restore(
    client: &Client,
    strain: &str,
    id: &str,
    save_current: bool,
    dry_run: bool,
) -> Result<RestoreOutcome, String> {
    let mut options: Dict = HashMap::new();
    let mut put_bool = |k: &str, v: bool| -> Result<(), String> {
        let owned = zvariant::Value::new(v)
            .try_to_owned()
            .map_err(|e| format!("encode option {k}: {e}"))?;
        options.insert(k.to_string(), owned);
        Ok(())
    };
    put_bool("save_current", save_current)?;
    put_bool("dry_run", dry_run)?;

    let raw = client
        .proxy()
        .restore(strain, id, options)
        .await
        .map_err(|e| format!("{e}"))?;
    RestoreOutcome::from_dict(&raw)
        .ok_or_else(|| "daemon returned malformed Restore result dict".to_string())
}
