//! D-Bus interface implementation for `org.revenant.Daemon1`.
//!
//! All methods are stubs. Each returns a `NotImplemented` error so the
//! GUI can be wired up against a real bus name and develop its proxy
//! code while the actual logic is being filled in.
//!
//! See `docs/design/dbus-interface.md` for the full contract.

use std::collections::HashMap;

use zbus::{fdo, interface};
use zvariant::{OwnedObjectPath, OwnedValue, Value};

/// Extensible-dict D-Bus type. `a{sv}` on the wire.
type Dict = HashMap<String, OwnedValue>;

/// Strain wire type — `(sasba{sv})`.
type StrainTuple = (String, Vec<String>, bool, Dict);

pub struct Daemon {
    // Real state lands here later:
    //   - cached snapshot index, refreshed by inotify
    //   - mutex around write-ops
    //   - polkit authority handle
    //   - mount lifecycle for /run/revenant/toplevel
}

impl Daemon {
    pub fn new() -> Self {
        Self {}
    }
}

#[interface(name = "org.revenant.Daemon1")]
impl Daemon {
    // -- Discovery / metadata ------------------------------------------

    async fn get_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    async fn get_daemon_info(&self) -> fdo::Result<Dict> {
        let mut out = Dict::new();
        let version: OwnedValue = Value::new(env!("CARGO_PKG_VERSION"))
            .try_to_owned()
            .map_err(|e| fdo::Error::Failed(format!("encode version: {e}")))?;
        out.insert("version".into(), version);
        Ok(out)
    }

    // -- Strains -------------------------------------------------------

    async fn list_strains(&self) -> fdo::Result<Vec<StrainTuple>> {
        Err(not_implemented("ListStrains"))
    }

    async fn get_strain(&self, _name: &str) -> fdo::Result<StrainTuple> {
        Err(not_implemented("GetStrain"))
    }

    async fn set_strain_retention(&self, _name: &str, _retention: Dict) -> fdo::Result<()> {
        Err(not_implemented("SetStrainRetention"))
    }

    // -- Snapshots -----------------------------------------------------

    async fn list_snapshots(&self, _filter: Dict) -> fdo::Result<Vec<Dict>> {
        Err(not_implemented("ListSnapshots"))
    }

    async fn get_snapshot(&self, _strain: &str, _id: &str) -> fdo::Result<Dict> {
        Err(not_implemented("GetSnapshot"))
    }

    async fn create_snapshot(&self, _strain: &str, _message: &str) -> fdo::Result<OwnedObjectPath> {
        Err(not_implemented("CreateSnapshot"))
    }

    async fn delete_snapshot(&self, _strain: &str, _id: &str) -> fdo::Result<OwnedObjectPath> {
        Err(not_implemented("DeleteSnapshot"))
    }

    // -- Live state ----------------------------------------------------

    async fn get_live_parent(&self) -> fdo::Result<Dict> {
        Err(not_implemented("GetLiveParent"))
    }

    // -- Restore -------------------------------------------------------

    async fn restore(
        &self,
        _strain: &str,
        _id: &str,
        _options: Dict,
    ) -> fdo::Result<OwnedObjectPath> {
        Err(not_implemented("Restore"))
    }

    // -- Signals -------------------------------------------------------

    #[zbus(signal)]
    async fn snapshots_changed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        strain: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn strain_config_changed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn live_parent_changed(ctxt: &zbus::object_server::SignalEmitter<'_>)
    -> zbus::Result<()>;

    #[zbus(signal)]
    async fn daemon_state_changed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        state: &str,
        message: &str,
    ) -> zbus::Result<()>;
}

fn not_implemented(method: &str) -> fdo::Error {
    fdo::Error::Failed(format!("{method} not implemented"))
}
