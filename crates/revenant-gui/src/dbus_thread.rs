//! Bridge between the GTK4 main loop (glib `MainContext`) and the
//! tokio runtime that drives the zbus client.
//!
//! Architecture: a dedicated OS thread owns a current-thread tokio
//! runtime. The runtime hosts the daemon `Client` and runs an event
//! loop that connects, fetches initial state, and pushes events back
//! across an `async-channel`. The GTK side `await`s receiver
//! events on the glib executor and updates widgets.
//!
//! Why a worker thread: zbus is built on tokio; glib has its own
//! executor. Polling tokio futures from glib (or vice-versa) is
//! brittle. Two single-threaded runtimes connected by a runtime-
//! agnostic channel is the predictable shape.
//!
//! Reconnect scope for slice 4a: retry the initial bus connect with
//! a fixed 5 s backoff. Once connected, the worker pushes the
//! initial `GetDaemonInfo` and parks. Detecting daemon death mid-
//! session and rebuilding the proxy is deferred to the signal slice
//! where it actually matters.

use std::time::Duration;

use async_channel::{Receiver, Sender, bounded};

use crate::client::Client;
use crate::proxy::Dict;

/// Events pushed from the worker to the GUI thread. Never flows
/// the other way for now — see module docs.
#[derive(Debug)]
pub enum Event {
    /// Worker has a live `Client` against the system bus.
    Connected,
    /// Connect attempt failed; the worker will keep retrying.
    /// Carries the human-readable reason for the latest attempt.
    Disconnected(String),
    /// Result of the initial `GetDaemonInfo` call.
    DaemonInfo(Result<Dict, String>),
}

/// Spawn the worker thread. Returns the event receiver the GUI
/// drains on the glib `MainContext`. Dropping the receiver does
/// not stop the worker — it'll just block forever on `send`. For
/// slice 4a that's fine; the worker dies with the process.
pub fn spawn() -> Receiver<Event> {
    let (evt_tx, evt_rx) = bounded::<Event>(16);

    std::thread::Builder::new()
        .name("revenant-dbus".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread tokio runtime");
            rt.block_on(run_loop(evt_tx));
        })
        .expect("spawn revenant-dbus thread");

    evt_rx
}

async fn run_loop(evt_tx: Sender<Event>) {
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

    let info = client
        .proxy()
        .get_daemon_info()
        .await
        .map_err(|e| format!("{e}"));
    let _ = evt_tx.send(Event::DaemonInfo(info)).await;

    // Park until the process exits. Subsequent slices will turn this
    // into a signal-subscription loop that pushes change events.
    std::future::pending::<()>().await;
}
