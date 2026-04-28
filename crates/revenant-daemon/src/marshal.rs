//! Convert `revenant-core` types into D-Bus wire types.
//!
//! All conversions go through here so the wire format lives in one
//! place. See `docs/design/dbus-interface.md` for the contract.

use std::collections::HashMap;

use revenant_core::metadata::{SnapshotMetadata, TriggerKind};
use revenant_core::{LiveParentRef, RetainConfig, SnapshotInfo, StrainConfig};
use zvariant::{OwnedValue, Value};

use crate::errors::{DaemonError, DaemonResult};

/// `a{sv}` extensible dict.
pub type Dict = HashMap<String, OwnedValue>;

/// Strain wire type — `(sasba{sv}s)`.
///
/// Tuple positions: name, subvolumes, efi, retain dict, display_name.
/// `display_name` is the empty string when unset in config; the GUI
/// treats `""` and absent identically.
pub type StrainTuple = (String, Vec<String>, bool, Dict, String);

pub fn strain_to_tuple(name: &str, cfg: &StrainConfig) -> DaemonResult<StrainTuple> {
    Ok((
        name.to_string(),
        cfg.subvolumes.clone(),
        cfg.efi,
        retain_to_dict(&cfg.retain)?,
        cfg.display_name.clone().unwrap_or_default(),
    ))
}

pub fn retain_to_dict(r: &RetainConfig) -> DaemonResult<Dict> {
    let mut d = Dict::new();
    insert_u32(&mut d, "last", to_u32(r.last))?;
    insert_u32(&mut d, "hourly", to_u32(r.hourly))?;
    insert_u32(&mut d, "daily", to_u32(r.daily))?;
    insert_u32(&mut d, "weekly", to_u32(r.weekly))?;
    insert_u32(&mut d, "monthly", to_u32(r.monthly))?;
    insert_u32(&mut d, "yearly", to_u32(r.yearly))?;
    Ok(d)
}

pub fn snapshot_to_dict(
    snap: &SnapshotInfo,
    live_parent: Option<&LiveParentRef>,
) -> DaemonResult<Dict> {
    let mut d = Dict::new();
    insert_str(&mut d, "id", snap.id.as_str())?;
    insert_str(&mut d, "strain", &snap.strain)?;

    // Prefer the sidecar's wall-clock with offset; fall back to the
    // UTC timestamp embedded in the id when the sidecar is missing or
    // unparsable.
    let created = snap
        .metadata
        .as_ref()
        .map(|m| m.created_at.to_rfc3339())
        .or_else(|| snap.id.created_at().map(|ts| ts.to_rfc3339()));
    if let Some(c) = created {
        insert_str(&mut d, "created", &c)?;
    }

    let (trigger, message) = trigger_and_message(snap.metadata.as_ref());
    insert_str(&mut d, "trigger", trigger)?;
    if let Some(msg) = message {
        insert_str(&mut d, "message", msg)?;
    }

    let is_anchor = matches!(
        live_parent,
        Some(lp) if lp.id == snap.id && lp.strain == snap.strain
    );
    insert_bool(&mut d, "is_live_anchor", is_anchor)?;

    Ok(d)
}

pub fn live_parent_to_dict(lp: &LiveParentRef) -> DaemonResult<Dict> {
    let mut d = Dict::new();
    insert_str(&mut d, "strain", &lp.strain)?;
    insert_str(&mut d, "id", lp.id.as_str())?;
    Ok(d)
}

fn trigger_and_message(meta: Option<&SnapshotMetadata>) -> (&'static str, Option<&str>) {
    let Some(meta) = meta else {
        return ("unknown", None);
    };
    let trigger = match meta.trigger.kind {
        TriggerKind::Manual => "manual",
        TriggerKind::Pacman => "pacman",
        TriggerKind::SystemdBoot => "systemd-boot",
        TriggerKind::SystemdPeriodic => "systemd-periodic",
        TriggerKind::Restore => "restore",
        TriggerKind::Unknown => "unknown",
    };
    (trigger, meta.message.as_deref())
}

// ---- low-level encoding helpers ----------------------------------------

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

pub fn insert_str(dict: &mut Dict, key: &str, value: &str) -> DaemonResult<()> {
    let v: OwnedValue = Value::new(value)
        .try_to_owned()
        .map_err(|e| DaemonError::Internal(format!("encode {key}: {e}")))?;
    dict.insert(key.to_string(), v);
    Ok(())
}

pub fn insert_bool(dict: &mut Dict, key: &str, value: bool) -> DaemonResult<()> {
    let v: OwnedValue = Value::new(value)
        .try_to_owned()
        .map_err(|e| DaemonError::Internal(format!("encode {key}: {e}")))?;
    dict.insert(key.to_string(), v);
    Ok(())
}

pub fn insert_u32(dict: &mut Dict, key: &str, value: u32) -> DaemonResult<()> {
    let v: OwnedValue = Value::new(value)
        .try_to_owned()
        .map_err(|e| DaemonError::Internal(format!("encode {key}: {e}")))?;
    dict.insert(key.to_string(), v);
    Ok(())
}
