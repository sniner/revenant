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
//! death mid-session and rebuilding the proxy is deferred to the
//! signal slice (4f) where it actually matters.

use std::collections::HashMap;
use std::time::Duration;

use async_channel::{Receiver, Sender, bounded, unbounded};

use crate::client::Client;
use crate::model::{LiveParent, Snapshot, Strain};
use crate::proxy::Dict;

/// Events pushed from the worker to the GUI thread.
#[derive(Debug)]
pub enum Event {
    /// Worker has a live `Client` against the system bus.
    Connected,
    /// Connect attempt failed; the worker will keep retrying.
    /// Carries the human-readable reason for the latest attempt.
    Disconnected(String),
    /// Result of the initial `GetDaemonInfo` call.
    DaemonInfo(Result<Dict, String>),
    /// Result of `ListStrains`.
    Strains(Result<Vec<Strain>, String>),
    /// Result of `GetLiveParent`. `Ok(None)` is the empty-dict
    /// sentinel ("pristine system / anchor lost").
    LiveParent(Result<Option<LiveParent>, String>),
    /// Result of `ListSnapshots(strain)`. The strain is echoed back
    /// so the GUI can route the result to the right list even if the
    /// user has moved on in the meantime.
    Snapshots {
        strain: String,
        result: Result<Vec<Snapshot>, String>,
    },
}

/// Commands sent from the GUI to the worker.
#[derive(Debug)]
pub enum Command {
    /// Fetch the snapshot list for a strain. Worker replies with
    /// `Event::Snapshots`.
    LoadSnapshots(String),
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

    fetch_initial(&client, &evt_tx).await;

    while let Ok(cmd) = cmd_rx.recv().await {
        handle_command(&client, &evt_tx, cmd).await;
    }
}

async fn fetch_initial(client: &Client, evt_tx: &Sender<Event>) {
    let info = client
        .proxy()
        .get_daemon_info()
        .await
        .map_err(|e| format!("{e}"));
    let _ = evt_tx.send(Event::DaemonInfo(info)).await;

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
}

async fn handle_command(client: &Client, evt_tx: &Sender<Event>, cmd: Command) {
    match cmd {
        Command::LoadSnapshots(strain) => {
            let result = list_snapshots_for(client, &strain).await;
            let _ = evt_tx.send(Event::Snapshots { strain, result }).await;
        }
    }
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
