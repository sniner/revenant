//! D-Bus interface implementation for `org.revenant.Daemon1`.
//!
//! Slices implemented so far:
//! 1. Mount lifecycle + `GetVersion` / `GetDaemonInfo`.
//! 2. Read-only path: `ListStrains`, `GetStrain`, `ListSnapshots`,
//!    `GetSnapshot`, `GetLiveParent`.
//! 3. Inotify watchers + `SnapshotsChanged` / `StrainConfigChanged`.
//! 4. `SetStrainRetention` with polkit + atomic config edit.
//! 5. `CreateSnapshot` / `DeleteSnapshot`, both synchronous.
//! 6. `Restore` (synchronous, with `save_current` + preflight +
//!    `LiveParentChanged`).
//!
//! See `docs/design/dbus-interface.md` for the wire-level contract.

use std::sync::Arc;

use std::path::Path;

use revenant_core::check::{Finding, Severity};
use revenant_core::cleanup::{list_delete_markers, purge_delete_markers_by_name};
use revenant_core::metadata::TriggerKind;
use revenant_core::preflight::{MACHINED_RUNTIME_DIR, preflight_restore};
use revenant_core::restore::restore_snapshot as core_restore_snapshot;
use revenant_core::snapshot::{
    create_snapshot as core_create_snapshot, delete_snapshot as core_delete_snapshot,
    discover_snapshots, find_snapshot, resolve_live_parent,
};
use revenant_core::{RetainConfig, RevenantError, SnapshotId};
use zbus::interface;
use zvariant::Value;

use crate::config_edit;
use crate::errors::DaemonError;
use crate::marshal::{
    self, Dict, StrainTuple, delete_marker_to_dict, live_parent_to_dict, snapshot_to_dict,
    strain_to_tuple,
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

    async fn get_daemon_info(&self) -> Result<Dict, DaemonError> {
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

    async fn list_strains(&self) -> Result<Vec<StrainTuple>, DaemonError> {
        let ready = self.state.ready().await?;
        let mut out: Vec<StrainTuple> = ready
            .config()
            .strain
            .iter()
            .map(|(name, sc)| strain_to_tuple(name, sc))
            .collect::<Result<_, DaemonError>>()?;
        // Stable order so clients don't have to re-sort.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn get_strain(&self, name: &str) -> Result<StrainTuple, DaemonError> {
        let ready = self.state.ready().await?;
        let sc = ready
            .config()
            .strain
            .get(name)
            .ok_or_else(|| DaemonError::NotFound(format!("unknown strain: {name}")))?;
        strain_to_tuple(name, sc)
    }

    async fn set_strain_retention(
        &self,
        name: &str,
        retention: Dict,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> Result<(), DaemonError> {
        let sender = hdr
            .sender()
            .ok_or_else(|| DaemonError::Internal("method call has no sender".into()))?;
        polkit::check(conn, sender.as_str(), "org.revenant.config.edit").await?;

        // Validate the strain exists before we touch the file.
        {
            let cfg_guard = self.state.config.read().await;
            let cfg = cfg_guard
                .as_ref()
                .ok_or_else(|| DaemonError::BackendUnavailable("config not loaded".into()))?;
            if !cfg.strain.contains_key(name) {
                return Err(DaemonError::NotFound(format!("unknown strain: {name}")));
            }
        }

        let retain = parse_retain(&retention)?;
        config_edit::set_strain_retention(name, &retain)
            .map_err(|e| DaemonError::Internal(format!("config edit: {e:#}")))?;

        // Pick up the new values into the in-memory config so
        // subsequent reads see the change immediately. The watcher
        // will also fire `StrainConfigChanged` on the file write,
        // which is how unprivileged clients learn about it.
        self.state
            .reload_config()
            .await
            .map_err(|e| DaemonError::Internal(format!("reload config: {e:#}")))?;

        Ok(())
    }

    /// Strain that holds the most recently created snapshot. Empty
    /// string if no strain has any snapshot yet. Used by the GUI to
    /// pick a sensible initial selection on launch.
    async fn get_latest_strain(&self) -> Result<String, DaemonError> {
        let ready = self.state.ready().await?;
        let snapshots = discover_snapshots(ready.config(), &self.state.backend, ready.toplevel())
            .map_err(map_core_error)?;
        // SnapshotInfo carries no created-at directly when the sidecar
        // is missing, but the id encodes a UTC timestamp — so falling
        // back on it keeps the comparison meaningful.
        Ok(snapshots
            .iter()
            .max_by_key(|s| {
                s.metadata
                    .as_ref()
                    .map(|m| m.created_at.to_utc())
                    .or_else(|| s.id.created_at())
            })
            .map(|s| s.strain.clone())
            .unwrap_or_default())
    }

    // -- Snapshots -----------------------------------------------------

    async fn list_snapshots(&self, filter: Dict) -> Result<Vec<Dict>, DaemonError> {
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

    async fn get_snapshot(&self, strain: &str, id: &str) -> Result<Dict, DaemonError> {
        let ready = self.state.ready().await?;
        let snap_id = SnapshotId::from_string(id)
            .map_err(|e| DaemonError::InvalidArgument(format!("invalid snapshot id {id}: {e}")))?;
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

    async fn create_snapshot(
        &self,
        strain: &str,
        message: Vec<String>,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> Result<Dict, DaemonError> {
        let sender = hdr
            .sender()
            .ok_or_else(|| DaemonError::Internal("method call has no sender".into()))?;
        polkit::check(conn, sender.as_str(), "org.revenant.snapshot.create").await?;

        let ready = self.state.ready().await?;
        let info = core_create_snapshot(
            ready.config(),
            &self.state.backend,
            ready.toplevel(),
            strain,
            TriggerKind::Manual,
            message,
        )
        .map_err(map_core_error)?;

        let live = resolve_live_parent(ready.config(), &self.state.backend, ready.toplevel());
        snapshot_to_dict(&info, live.as_ref())
    }

    async fn delete_snapshot(
        &self,
        strain: &str,
        id: &str,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> Result<(), DaemonError> {
        let sender = hdr
            .sender()
            .ok_or_else(|| DaemonError::Internal("method call has no sender".into()))?;
        polkit::check(conn, sender.as_str(), "org.revenant.snapshot.delete").await?;

        let ready = self.state.ready().await?;
        let snap_id = SnapshotId::from_string(id)
            .map_err(|e| DaemonError::InvalidArgument(format!("invalid snapshot id {id}: {e}")))?;
        let snapshot = find_snapshot(
            ready.config(),
            &self.state.backend,
            ready.toplevel(),
            &snap_id,
            Some(strain),
        )
        .map_err(map_core_error)?;
        core_delete_snapshot(
            ready.config(),
            &self.state.backend,
            ready.toplevel(),
            &snapshot,
        )
        .map_err(map_core_error)?;
        Ok(())
    }

    // -- Pre-restore states (DELETE markers) ---------------------------

    /// Enumerate `<base>-DELETE-<ts>` subvolumes left over from earlier
    /// restores. These are not snapshots — they're the previous live
    /// state, renamed at restore time as the user's safety net. Unlike
    /// snapshots they accumulate until an explicit cleanup; the GUI
    /// surfaces them via this method.
    async fn list_delete_markers(&self) -> Result<Vec<Dict>, DaemonError> {
        let ready = self.state.ready().await?;
        let markers = list_delete_markers(ready.config(), &self.state.backend, ready.toplevel())
            .map_err(map_core_error)?;
        markers.iter().map(delete_marker_to_dict).collect()
    }

    /// Purge a user-chosen set of `<base>-DELETE-<ts>` markers.
    ///
    /// Returns the names that were actually removed. Names that don't
    /// match any current marker are silently dropped from the result —
    /// a concurrent CLI cleanup could have purged them between the
    /// GUI's listing and the user's confirmation, and that's fine.
    async fn purge_delete_markers(
        &self,
        names: Vec<String>,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(signal_emitter)] signal_emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> Result<Vec<String>, DaemonError> {
        let sender = hdr
            .sender()
            .ok_or_else(|| DaemonError::Internal("method call has no sender".into()))?;
        polkit::check(conn, sender.as_str(), "org.revenant.cleanup").await?;

        let ready = self.state.ready().await?;
        let removed = purge_delete_markers_by_name(
            ready.config(),
            &self.state.backend,
            ready.toplevel(),
            &names,
        )
        .map_err(map_core_error)?;

        if !removed.is_empty() {
            if let Err(e) = Self::delete_markers_changed(&signal_emitter).await {
                tracing::warn!("emit DeleteMarkersChanged after purge: {e}");
            }
        }

        Ok(removed)
    }

    // -- Live state ----------------------------------------------------

    async fn get_live_parent(&self) -> Result<Dict, DaemonError> {
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
        strain: &str,
        id: &str,
        options: Dict,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(signal_emitter)] signal_emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> Result<Dict, DaemonError> {
        let sender = hdr
            .sender()
            .ok_or_else(|| DaemonError::Internal("method call has no sender".into()))?;
        polkit::check(conn, sender.as_str(), "org.revenant.restore").await?;

        let save_current = read_bool_opt(&options, "save_current").unwrap_or(true);
        let dry_run = read_bool_opt(&options, "dry_run").unwrap_or(false);

        let ready = self.state.ready().await?;
        let snap_id = SnapshotId::from_string(id)
            .map_err(|e| DaemonError::InvalidArgument(format!("invalid snapshot id {id}: {e}")))?;
        let snapshot = find_snapshot(
            ready.config(),
            &self.state.backend,
            ready.toplevel(),
            &snap_id,
            Some(strain),
        )
        .map_err(map_core_error)?;

        // Preflight: blocking findings (Severity::Error) abort the
        // restore. The CLI offers --force; the GUI surfaces the
        // findings and asks the user to fix the underlying issue
        // before retrying. No --force in the daemon for now.
        let findings = preflight_restore(Path::new(MACHINED_RUNTIME_DIR));
        let blocking: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .collect();
        if !dry_run && !blocking.is_empty() {
            let summary = blocking
                .iter()
                .map(|f| format!("{}: {}", f.check, f.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(DaemonError::PreflightBlocked(summary));
        }

        let mut out = Dict::new();
        marshal::insert_str(&mut out, "restored_id", id)?;
        marshal::insert_str(&mut out, "restored_strain", strain)?;
        marshal::insert_bool(&mut out, "dry_run", dry_run)?;
        attach_findings(&mut out, &findings)?;

        if dry_run {
            return Ok(out);
        }

        let pre_restore = if save_current {
            // Mirror the CLI: --save-current creates a strain-internal
            // snapshot tagged with `TriggerKind::Restore` and the source
            // snapshot id as the message. Retention is deliberately not
            // applied — restore is an exceptional operation and the
            // source snapshot must survive the safety snapshot.
            // If this fails we abort *before* touching live subvolumes.
            let info = core_create_snapshot(
                ready.config(),
                &self.state.backend,
                ready.toplevel(),
                strain,
                TriggerKind::Restore,
                vec![snapshot.id.to_string()],
            )
            .map_err(map_core_error)?;
            Some(info)
        } else {
            None
        };

        core_restore_snapshot(
            ready.config(),
            &self.state.backend,
            ready.toplevel(),
            &snapshot,
        )
        .map_err(map_core_error)?;

        if let Some(pre) = &pre_restore {
            marshal::insert_str(&mut out, "pre_restore_id", pre.id.as_str())?;
            marshal::insert_str(&mut out, "pre_restore_strain", &pre.strain)?;
        }

        // Tell subscribers the live parent moved. SnapshotsChanged
        // also fires (via the inotify watcher catching the new
        // subvolumes), but it doesn't carry "the anchor changed" —
        // that's what LiveParentChanged is for.
        if let Err(e) = Self::live_parent_changed(&signal_emitter).await {
            tracing::warn!("emit LiveParentChanged after restore: {e}");
        }
        // Restore renames the previous live subvolume(s) into a new
        // `<base>-DELETE-<ts>` marker — the GUI's "pre-restore states"
        // count goes up by one.
        if let Err(e) = Self::delete_markers_changed(&signal_emitter).await {
            tracing::warn!("emit DeleteMarkersChanged after restore: {e}");
        }

        Ok(out)
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

    #[zbus(signal)]
    pub async fn delete_markers_changed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
}

/// Map a `revenant-core` error onto the closest custom D-Bus error.
/// Categories follow the wire-error table in the design doc.
fn map_core_error(err: RevenantError) -> DaemonError {
    match err {
        RevenantError::SnapshotNotFound(_) | RevenantError::SubvolumeNotFound(_) => {
            DaemonError::NotFound(err.to_string())
        }
        RevenantError::Config(_) => DaemonError::InvalidArgument(err.to_string()),
        RevenantError::NotBtrfs { .. } | RevenantError::Mount(_) => {
            DaemonError::BackendUnavailable(err.to_string())
        }
        RevenantError::NotRoot => DaemonError::NotAuthorized(err.to_string()),
        _ => DaemonError::Internal(err.to_string()),
    }
}

/// Read an optional `bool` option from an extensible-dict argument.
/// Returns `None` if the key is missing, `Some(b)` if it parses, an
/// error if the key is present with a non-bool value.
fn read_bool_opt(d: &Dict, key: &str) -> Option<bool> {
    d.get(key).and_then(|v| bool::try_from(v).ok())
}

/// Append `findings` as `aa{sv}` under the `findings` key. Always
/// emits the key, even for an empty array — the GUI can rely on its
/// presence rather than checking for absence.
fn attach_findings(out: &mut Dict, findings: &[Finding]) -> Result<(), DaemonError> {
    let encoded: Vec<Dict> = findings
        .iter()
        .map(|f| {
            let mut d = Dict::new();
            marshal::insert_str(&mut d, "severity", f.severity.label())?;
            marshal::insert_str(&mut d, "check", f.check)?;
            marshal::insert_str(&mut d, "message", &f.message)?;
            if let Some(hint) = &f.hint {
                marshal::insert_str(&mut d, "hint", hint)?;
            }
            Ok::<_, DaemonError>(d)
        })
        .collect::<Result<_, DaemonError>>()?;
    let value = Value::from(encoded)
        .try_to_owned()
        .map_err(|e| DaemonError::Internal(format!("encode findings: {e}")))?;
    out.insert("findings".to_string(), value);
    Ok(())
}

/// Decode a retention dict (`a{sv}`) into a [`RetainConfig`]. Missing
/// keys default to 0 (= disabled), per the IDL.
fn parse_retain(d: &Dict) -> Result<RetainConfig, DaemonError> {
    fn read_tier(d: &Dict, key: &str) -> Result<usize, DaemonError> {
        match d.get(key) {
            None => Ok(0),
            Some(v) => u32::try_from(v)
                .map(|n| n as usize)
                .map_err(|e| DaemonError::InvalidArgument(format!("{key}: not a u32: {e}"))),
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
