//! D-Bus interface implementation for `org.revenant.Daemon1`.
//!
//! Slice 1 wires up read-only metadata (`GetVersion`, `GetDaemonInfo`)
//! against the live [`DaemonState`]. Everything else still returns
//! `NotImplemented`. See `docs/design/dbus-interface.md`.

use std::collections::HashMap;
use std::sync::Arc;

use zbus::{fdo, interface};
use zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::state::DaemonState;

/// Extensible-dict D-Bus type. `a{sv}` on the wire.
type Dict = HashMap<String, OwnedValue>;

/// Strain wire type — `(sasba{sv})`.
type StrainTuple = (String, Vec<String>, bool, Dict);

pub struct Daemon {
    state: Arc<DaemonState>,
}

impl Daemon {
    pub fn new(state: Arc<DaemonState>) -> Self {
        Self { state }
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
        insert_str(&mut out, "version", env!("CARGO_PKG_VERSION"))?;
        insert_str(&mut out, "backend", self.state.backend_name())?;
        insert_bool(&mut out, "toplevel_mounted", self.state.toplevel.is_some())?;

        if let Some(mount) = &self.state.toplevel {
            insert_str(&mut out, "toplevel_path", &mount.path().to_string_lossy())?;
        }
        if let Some(cfg) = &self.state.config {
            insert_str(&mut out, "device_uuid", &cfg.sys.rootfs.device_uuid)?;
        }
        if let Some(reason) = &self.state.degraded {
            insert_str(&mut out, "degraded_reason", &reason.to_string())?;
        }
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

fn insert_str(dict: &mut Dict, key: &str, value: &str) -> fdo::Result<()> {
    let v: OwnedValue = Value::new(value)
        .try_to_owned()
        .map_err(|e| fdo::Error::Failed(format!("encode {key}: {e}")))?;
    dict.insert(key.to_string(), v);
    Ok(())
}

fn insert_bool(dict: &mut Dict, key: &str, value: bool) -> fdo::Result<()> {
    let v: OwnedValue = Value::new(value)
        .try_to_owned()
        .map_err(|e| fdo::Error::Failed(format!("encode {key}: {e}")))?;
    dict.insert(key.to_string(), v);
    Ok(())
}
