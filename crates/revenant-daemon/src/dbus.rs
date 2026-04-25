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
use revenant_core::{RetainConfig, RevenantError, SnapshotId};
use zbus::{fdo, interface};
use zvariant::OwnedObjectPath;

use crate::config_edit;
use crate::marshal::{
    self, Dict, StrainTuple, live_parent_to_dict, snapshot_to_dict, strain_to_tuple,
};
use crate::polkit;
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
        let config = self.state.config.read().await;
        if let Some(cfg) = config.as_ref() {
            marshal::insert_str(&mut out, "device_uuid", &cfg.sys.rootfs.device_uuid)?;
        }
        drop(config);
        if let Some(reason) = &self.state.degraded {
            marshal::insert_str(&mut out, "degraded_reason", &reason.to_string())?;
        }
        Ok(out)
    }

    // -- Strains -------------------------------------------------------

    async fn list_strains(&self) -> fdo::Result<Vec<StrainTuple>> {
        let ready = self.state.ready().await?;
        let mut out: Vec<StrainTuple> = ready
            .config()
            .strain
            .iter()
            .map(|(name, sc)| strain_to_tuple(name, sc))
            .collect::<fdo::Result<_>>()?;
        // Stable order so clients don't have to re-sort.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn get_strain(&self, name: &str) -> fdo::Result<StrainTuple> {
        let ready = self.state.ready().await?;
        let sc = ready
            .config()
            .strain
            .get(name)
            .ok_or_else(|| fdo::Error::Failed(format!("unknown strain: {name}")))?;
        strain_to_tuple(name, sc)
    }

    async fn set_strain_retention(
        &self,
        name: &str,
        retention: Dict,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> fdo::Result<()> {
        let sender = hdr
            .sender()
            .ok_or_else(|| fdo::Error::Failed("method call has no sender".into()))?;
        polkit::check(conn, sender.as_str(), "org.revenant.config.edit").await?;

        // Validate the strain exists before we touch the file.
        {
            let cfg_guard = self.state.config.read().await;
            let cfg = cfg_guard
                .as_ref()
                .ok_or_else(|| fdo::Error::Failed("config not loaded".into()))?;
            if !cfg.strain.contains_key(name) {
                return Err(fdo::Error::Failed(format!("unknown strain: {name}")));
            }
        }

        let retain = parse_retain(&retention)?;
        config_edit::set_strain_retention(name, &retain)
            .map_err(|e| fdo::Error::Failed(format!("config edit: {e:#}")))?;

        // Pick up the new values into the in-memory config so
        // subsequent reads see the change immediately. The watcher
        // will also fire `StrainConfigChanged` on the file write,
        // which is how unprivileged clients learn about it.
        self.state
            .reload_config()
            .await
            .map_err(|e| fdo::Error::Failed(format!("reload config: {e:#}")))?;

        Ok(())
    }

    // -- Snapshots -----------------------------------------------------

    async fn list_snapshots(&self, filter: Dict) -> fdo::Result<Vec<Dict>> {
        let ready = self.state.ready().await?;
        let strain_filter = filter
            .get("strain")
            .and_then(|v| <&str>::try_from(v).ok())
            .map(str::to_owned);

        let snapshots = discover_snapshots(ready.config(), &self.state.backend, ready.toplevel())
            .map_err(map_core_error)?;
        let live = resolve_live_parent(ready.config(), &self.state.backend, ready.toplevel());

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
        let ready = self.state.ready().await?;
        let snap_id = SnapshotId::from_string(id)
            .map_err(|e| fdo::Error::InvalidArgs(format!("invalid snapshot id {id}: {e}")))?;
        let snap = find_snapshot(
            ready.config(),
            &self.state.backend,
            ready.toplevel(),
            &snap_id,
            Some(strain),
        )
        .map_err(map_core_error)?;
        let live = resolve_live_parent(ready.config(), &self.state.backend, ready.toplevel());
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
        let ready = self.state.ready().await?;
        match resolve_live_parent(ready.config(), &self.state.backend, ready.toplevel()) {
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
    pub async fn snapshots_changed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        strain: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn strain_config_changed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn live_parent_changed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn daemon_state_changed(
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

/// Decode a retention dict (`a{sv}`) into a [`RetainConfig`]. Missing
/// keys default to 0 (= disabled), per the IDL.
fn parse_retain(d: &Dict) -> fdo::Result<RetainConfig> {
    fn read_tier(d: &Dict, key: &str) -> fdo::Result<usize> {
        match d.get(key) {
            None => Ok(0),
            Some(v) => u32::try_from(v)
                .map(|n| n as usize)
                .map_err(|e| fdo::Error::InvalidArgs(format!("{key}: not a u32: {e}"))),
        }
    }
    Ok(RetainConfig {
        last: read_tier(d, "last")?,
        hourly: read_tier(d, "hourly")?,
        daily: read_tier(d, "daily")?,
        weekly: read_tier(d, "weekly")?,
        monthly: read_tier(d, "monthly")?,
        yearly: read_tier(d, "yearly")?,
    })
}
