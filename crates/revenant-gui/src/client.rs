//! Connection wrapper around `org.revenant.Daemon1`.
//!
//! Owns the system-bus `Connection` and a typed `DaemonProxy` against
//! it. Higher layers (the worker thread in `dbus_thread`) call
//! `Client::connect` once on startup and then issue method calls on
//! the proxy.
//!
//! Reconnect semantics: on a fresh connect failure, the worker waits
//! and retries. Once connected, the underlying zbus connection
//! transparently survives the daemon being restarted on the same bus
//! (D-Bus activation re-spawns it on demand). A bus-level transport
//! drop, however, requires building a new `Client`.

use anyhow::{Context, Result};
use zbus::Connection;

use crate::proxy::DaemonProxy;

pub struct Client {
    proxy: DaemonProxy<'static>,
}

impl Client {
    /// Connect to the system bus and bind a proxy to
    /// `org.revenant.Daemon1`. Fails if the bus is unreachable;
    /// **does not** fail if the daemon is currently inactive — the
    /// proxy is bound to the well-known name, and the first method
    /// call will trigger D-Bus activation.
    pub async fn connect() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("connect to system bus")?;
        let proxy = DaemonProxy::new(&conn)
            .await
            .context("build org.revenant.Daemon1 proxy")?;
        Ok(Self { proxy })
    }

    pub fn proxy(&self) -> &DaemonProxy<'static> {
        &self.proxy
    }
}
