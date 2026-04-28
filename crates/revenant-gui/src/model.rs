//! Domain types parsed out of the daemon's `a{sv}`/tuple wire forms.
//!
//! The proxy layer (`crate::proxy`) returns raw `HashMap<String, OwnedValue>`
//! because that's the shape `zbus` deserialises a{sv} into. Decoding
//! into typed structs lives here so the UI never touches `OwnedValue`
//! directly.
//!
//! Decoders are tolerant: a missing optional key yields `None`/default,
//! and an unexpected type for a key results in `None`/default rather
//! than a hard error. The daemon controls both ends of the wire and
//! never violates the documented schema, but the GUI shouldn't crash
//! over a future field added behind it.

use crate::proxy::{Dict, StrainTuple};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strain {
    pub name: String,
    /// Optional human-readable label from `config.toml`. `None` when
    /// the wire string is empty.
    pub display_name: Option<String>,
    pub subvolumes: Vec<String>,
    pub efi: bool,
    pub retention: Retention,
}

impl Strain {
    /// Title to render in the sidebar — display_name when set,
    /// identifier otherwise.
    pub fn title(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

/// Tiered retention policy for one strain. All tiers are unsigned
/// counts; `0` disables a tier. Mirrors `RetainConfig` in core, but
/// kept independent so the GUI doesn't have to depend on the core
/// crate just for one struct.
///
/// Wire shape (per `dbus-interface.md`): each tier is a `u` in the
/// `retention` `a{sv}`; missing keys default to `0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Retention {
    pub last: u32,
    pub hourly: u32,
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
    pub yearly: u32,
}

impl Retention {
    fn from_dict(d: &Dict) -> Self {
        Self {
            last: read_u32(d, "last").unwrap_or(0),
            hourly: read_u32(d, "hourly").unwrap_or(0),
            daily: read_u32(d, "daily").unwrap_or(0),
            weekly: read_u32(d, "weekly").unwrap_or(0),
            monthly: read_u32(d, "monthly").unwrap_or(0),
            yearly: read_u32(d, "yearly").unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub strain: String,
    /// RFC 3339 timestamp; missing only for snapshots with neither a
    /// sidecar nor a parseable id. The daemon usually sends one.
    pub created: Option<String>,
    pub trigger: String,
    pub message: Option<String>,
    /// Human-readable summary line — trigger detail (pacman targets,
    /// systemd unit, restore target) optionally combined with the
    /// message. Mirrors the CLI's "Description" column minus the
    /// leading trigger kind. `None` when there is nothing to show.
    pub description: Option<String>,
    pub is_live_anchor: bool,
    /// True when retention currently keeps this snapshot. Drives the
    /// `Protected` pill in the detail pane.
    pub is_protected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveParent {
    pub strain: String,
    pub id: String,
}

impl Strain {
    pub fn from_tuple(t: StrainTuple) -> Self {
        let (name, subvolumes, efi, retain, display_name) = t;
        Self {
            name,
            display_name: if display_name.is_empty() {
                None
            } else {
                Some(display_name)
            },
            subvolumes,
            efi,
            retention: Retention::from_dict(&retain),
        }
    }
}

impl Snapshot {
    pub fn from_dict(d: &Dict) -> Option<Self> {
        let id = read_str(d, "id")?.to_string();
        let strain = read_str(d, "strain")?.to_string();
        Some(Self {
            id,
            strain,
            created: read_str(d, "created").map(str::to_owned),
            trigger: read_str(d, "trigger").unwrap_or("unknown").to_string(),
            message: read_str(d, "message").map(str::to_owned),
            description: read_str(d, "description").map(str::to_owned),
            is_live_anchor: read_bool(d, "is_live_anchor").unwrap_or(false),
            is_protected: read_bool(d, "is_protected").unwrap_or(false),
        })
    }
}

impl LiveParent {
    /// `Some` for a populated dict, `None` for the empty-dict sentinel
    /// the daemon returns when there is no resolvable parent.
    pub fn from_dict(d: &Dict) -> Option<Self> {
        if d.is_empty() {
            return None;
        }
        let strain = read_str(d, "strain")?.to_string();
        let id = read_str(d, "id")?.to_string();
        Some(Self { strain, id })
    }
}

/// Decoded form of the dict returned by `Restore(...)`. The wire
/// shape is documented in `docs/design/dbus-interface.md`; here we
/// keep only the bits the UI surfaces.
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub restored_strain: String,
    pub restored_id: String,
    /// `Some((strain, id))` when `save_current` was true and a
    /// pre-restore snapshot was created. The wireframe surfaces it
    /// so the user knows where to return to.
    pub pre_restore: Option<(String, String)>,
    /// True when the daemon ran in dry-run mode — no live state
    /// changed, only the preflight findings are meaningful.
    pub dry_run: bool,
}

impl RestoreOutcome {
    pub fn from_dict(d: &Dict) -> Option<Self> {
        let restored_strain = read_str(d, "restored_strain")?.to_string();
        let restored_id = read_str(d, "restored_id")?.to_string();
        let dry_run = read_bool(d, "dry_run").unwrap_or(false);
        let pre_restore = match (
            read_str(d, "pre_restore_strain"),
            read_str(d, "pre_restore_id"),
        ) {
            (Some(s), Some(i)) => Some((s.to_string(), i.to_string())),
            _ => None,
        };
        Some(Self {
            restored_strain,
            restored_id,
            pre_restore,
            dry_run,
        })
    }
}

fn read_str<'a>(dict: &'a Dict, key: &str) -> Option<&'a str> {
    dict.get(key).and_then(|v| <&str>::try_from(v).ok())
}

fn read_bool(dict: &Dict, key: &str) -> Option<bool> {
    dict.get(key).and_then(|v| bool::try_from(v).ok())
}

fn read_u32(dict: &Dict, key: &str) -> Option<u32> {
    dict.get(key).and_then(|v| u32::try_from(v).ok())
}
