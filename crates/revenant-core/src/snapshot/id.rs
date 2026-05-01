use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, NaiveDateTime, Timelike, Utc};
use serde::Serialize;

use crate::error::RevenantError;

/// Snapshot identifier based on UTC timestamp: `YYYYMMDD-HHMMSS-NNN`,
/// where the trailing three digits are milliseconds. Legacy IDs without
/// the millisecond suffix (`YYYYMMDD-HHMMSS`) are still accepted on read,
/// so existing snapshots remain valid; new IDs are always emitted in the
/// extended form.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SnapshotId(String);

/// Length of a legacy snapshot ID, `YYYYMMDD-HHMMSS`.
pub(super) const ID_LEN_LEGACY: usize = 15;

/// Length of the current snapshot ID, `YYYYMMDD-HHMMSS-NNN`.
pub(super) const ID_LEN_CURRENT: usize = 19;

impl SnapshotId {
    /// Generate a new snapshot ID from the current UTC time, including
    /// millisecond precision so that snapshots created back-to-back in
    /// the same strain do not collide.
    #[must_use]
    pub fn now() -> Self {
        let ts = Utc::now().format("%Y%m%d-%H%M%S-%3f").to_string();
        Self(ts)
    }

    /// Create a snapshot ID from a known string (e.g. parsed from subvolume name).
    /// Accepts both the legacy 15-char form and the current 19-char form.
    pub fn from_string(s: &str) -> std::result::Result<Self, RevenantError> {
        match s.len() {
            ID_LEN_CURRENT => {
                if s.as_bytes()[8] != b'-' || s.as_bytes()[15] != b'-' {
                    return Err(RevenantError::Other(format!(
                        "invalid snapshot ID format: {s}"
                    )));
                }
                let ms = &s[16..];
                if !ms.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(RevenantError::Other(format!(
                        "invalid snapshot ID milliseconds: {s}"
                    )));
                }
                NaiveDateTime::parse_from_str(&s[..ID_LEN_LEGACY], "%Y%m%d-%H%M%S").map_err(
                    |_| RevenantError::Other(format!("invalid snapshot ID timestamp: {s}")),
                )?;
            }
            ID_LEN_LEGACY => {
                if s.as_bytes()[8] != b'-' {
                    return Err(RevenantError::Other(format!(
                        "invalid snapshot ID format: {s}"
                    )));
                }
                NaiveDateTime::parse_from_str(s, "%Y%m%d-%H%M%S").map_err(|_| {
                    RevenantError::Other(format!("invalid snapshot ID timestamp: {s}"))
                })?;
            }
            _ => {
                return Err(RevenantError::Other(format!(
                    "invalid snapshot ID format: {s}"
                )));
            }
        }
        Ok(Self(s.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derive a UTC `DateTime` from the embedded timestamp. The
    /// millisecond suffix, if any, is preserved in the returned value.
    #[must_use]
    pub fn created_at(&self) -> Option<DateTime<Utc>> {
        let (secs, ms) = if self.0.len() == ID_LEN_CURRENT {
            (
                &self.0[..ID_LEN_LEGACY],
                self.0[ID_LEN_LEGACY + 1..].parse::<u32>().ok()?,
            )
        } else {
            (self.0.as_str(), 0)
        };
        let dt = NaiveDateTime::parse_from_str(secs, "%Y%m%d-%H%M%S")
            .ok()?
            .and_utc();
        dt.with_nanosecond(ms.checked_mul(1_000_000)?)
    }

    /// Build the snapshot subvolume name for a given source subvolume and strain.
    /// E.g. source "@", strain "default", id "20260316-143022-456" →
    /// "@-default-20260316-143022-456".
    #[must_use]
    pub fn snapshot_name(&self, subvol: &str, strain: &str) -> String {
        format!("{subvol}-{strain}-{}", self.0)
    }

    /// Extract a trailing snapshot ID from a string like
    /// `"...-<strain>-<id>"`. Tries the current 19-char form first, then
    /// the legacy 15-char form. Returns the parsed ID and the byte index
    /// in `s` at which the ID begins (the byte at `start - 1` is the
    /// `'-'` separator).
    #[must_use]
    pub fn extract_trailing(s: &str) -> Option<(Self, usize)> {
        for &width in &[ID_LEN_CURRENT, ID_LEN_LEGACY] {
            if s.len() < width + 2 {
                continue;
            }
            let start = s.len() - width;
            if s.as_bytes()[start - 1] != b'-' {
                continue;
            }
            if let Ok(id) = Self::from_string(&s[start..]) {
                return Some((id, start));
            }
        }
        None
    }
}

/// Parse a snapshot subvolume name back into its `(subvol, strain, id)`
/// components — the inverse of [`SnapshotId::snapshot_name`].
///
/// Strain names are constrained to `[a-zA-Z0-9_]` (no hyphens), so the
/// last `-` before the trailing id separator is the subvol/strain
/// boundary. Returns `None` for anything that isn't shaped like a
/// snapshot subvolume name.
#[must_use]
pub fn parse_snapshot_subvol_name(name: &str) -> Option<(String, String, SnapshotId)> {
    let (id, id_start) = SnapshotId::extract_trailing(name)?;
    // `prefix` is "<subvol>-<strain>"; id_start points at the byte after
    // the trailing '-' separator.
    let prefix = &name[..id_start - 1];
    let dash = prefix.rfind('-')?;
    let subvol = &prefix[..dash];
    let strain = &prefix[dash + 1..];
    if subvol.is_empty() || !crate::config::is_strain_token_lexical(strain) {
        return None;
    }
    Some((subvol.to_string(), strain.to_string(), id))
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0)
    }
}

impl FromStr for SnapshotId {
    type Err = RevenantError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::from_string(s)
    }
}
