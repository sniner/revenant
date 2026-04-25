//! D-Bus interface implementation for `org.revenant.Daemon1`.
//!
//! Slices implemented so far:
//! 1. Mount lifecycle + `GetVersion` / `GetDaemonInfo`.
//! 2. Read-only path: `ListStrains`, `GetStrain`, `ListSnapshots`,
//!    `GetSnapshot`, `GetLiveParent`.
//!
//! Privileged write-paths (`SetStrainRetention`, `CreateSnapshot`,
//! `DeleteSnapshot`, `Restore`) are still stubs. See
//! `docs/design/dbus-interface.md`.

use std::sync::Arc;

use revenant_core::snapshot::{discover_snapshots, find_snapshot, resolve_live_parent};
use revenant_core::{RevenantError, SnapshotId};
use zbus::{fdo, interface};
use zvariant::OwnedObjectPath;

use crate::marshal::{
    self, Dict, StrainTuple, live_parent_to_dict, snapshot_to_dict, strain_to_tuple,
};
use crate::state::DaemonState;

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
        marshal::insert_str(&mut out, "version", env!("CARGO_PKG_VERSION"))?;
        marshal::insert_str(&mut out, "backend", self.state.backend_name())?;
        marshal::insert_bool(&mut out, "toplevel_mounted", self.state.toplevel.is_some())?;

        if let Some(mount) = &self.state.toplevel {
            marshal::insert_str(&mut out, "toplevel_path", &mount.path().to_string_lossy())?;
        }
        if let Some(cfg) = &self.state.config {
            marshal::insert_str(&mut out, "device_uuid", &cfg.sys.rootfs.device_uuid)?;
        }
        if let Some(reason) = &self.state.degraded {
            marshal::insert_str(&mut out, "degraded_reason", &reason.to_string())?;
        }
        Ok(out)
    }

    // -- Strains -------------------------------------------------------

    async fn list_strains(&self) -> fdo::Result<Vec<StrainTuple>> {
        let (cfg, _toplevel) = self.state.ready()?;
        let mut out: Vec<StrainTuple> = cfg
            .strain
            .iter()
            .map(|(name, sc)| strain_to_tuple(name, sc))
            .collect::<fdo::Result<_>>()?;
        // Stable order so clients don't have to re-sort.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn get_strain(&self, name: &str) -> fdo::Result<StrainTuple> {
        let (cfg, _toplevel) = self.state.ready()?;
        let sc = cfg
            .strain
            .get(name)
            .ok_or_else(|| fdo::Error::Failed(format!("unknown strain: {name}")))?;
        strain_to_tuple(name, sc)
    }

    async fn set_strain_retention(&self, _name: &str, _retention: Dict) -> fdo::Result<()> {
        Err(not_implemented("SetStrainRetention"))
    }

    // -- Snapshots -----------------------------------------------------

    async fn list_snapshots(&self, filter: Dict) -> fdo::Result<Vec<Dict>> {
        let (cfg, toplevel) = self.state.ready()?;
        let strain_filter = filter
            .get("strain")
            .and_then(|v| <&str>::try_from(v).ok())
            .map(str::to_owned);

        let snapshots =
            discover_snapshots(cfg, &self.state.backend, toplevel).map_err(map_core_error)?;
        let live = resolve_live_parent(cfg, &self.state.backend, toplevel);

        snapshots
            .iter()
            .filter(|s| match &strain_filter {
                Some(want) => s.strain == *want,
                None => true,
            })
            .map(|s| snapshot_to_dict(s, live.as_ref()))
            .collect()
    }

    async fn get_snapshot(&self, strain: &str, id: &str) -> fdo::Result<Dict> {
        let (cfg, toplevel) = self.state.ready()?;
        let snap_id = SnapshotId::from_string(id)
            .map_err(|e| fdo::Error::InvalidArgs(format!("invalid snapshot id {id}: {e}")))?;
        let snap = find_snapshot(cfg, &self.state.backend, toplevel, &snap_id, Some(strain))
            .map_err(map_core_error)?;
        let live = resolve_live_parent(cfg, &self.state.backend, toplevel);
        snapshot_to_dict(&snap, live.as_ref())
    }

    async fn create_snapshot(&self, _strain: &str, _message: &str) -> fdo::Result<OwnedObjectPath> {
        Err(not_implemented("CreateSnapshot"))
    }

    async fn delete_snapshot(&self, _strain: &str, _id: &str) -> fdo::Result<OwnedObjectPath> {
        Err(not_implemented("DeleteSnapshot"))
    }

    // -- Live state ----------------------------------------------------

    async fn get_live_parent(&self) -> fdo::Result<Dict> {
        let (cfg, toplevel) = self.state.ready()?;
        match resolve_live_parent(cfg, &self.state.backend, toplevel) {
            Some(lp) => live_parent_to_dict(&lp),
            // Empty dict ≡ "no resolvable parent" per the IDL. Avoids
            // the awkward "Optional struct" wire type.
            None => Ok(Dict::new()),
        }
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

/// Map a `revenant-core` error onto the closest D-Bus error. Custom
/// `org.revenant.Error.*` errors will replace this once the error
/// infrastructure lands; for now everything that isn't an obvious
/// "not found" goes through `Failed`.
fn map_core_error(err: RevenantError) -> fdo::Error {
    match err {
        RevenantError::SnapshotNotFound(_) => fdo::Error::Failed(err.to_string()),
        RevenantError::Config(_) => fdo::Error::InvalidArgs(err.to_string()),
        _ => fdo::Error::Failed(err.to_string()),
    }
}
