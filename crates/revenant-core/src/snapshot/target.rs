use std::fmt;
use std::str::FromStr;

use crate::error::RevenantError;

use super::id::SnapshotId;

/// A user-supplied snapshot reference for `restore`/`delete`/`list`.
///
/// The textual form is the canonical addressing notation across the CLI:
///
/// * `strain@ID` — fully qualified single snapshot
/// * `ID` — single snapshot, strain auto-resolved by lookup
/// * `strain@` — every snapshot in a strain (bulk)
/// * `strain@all` — alias for `strain@`, kept because an empty ID slot
///   looks like a missing argument in scripts
///
/// Strain names are restricted to `[a-zA-Z0-9_]` (see
/// [`crate::config`]'s validation), so `@` is unambiguous as the
/// separator and we split on the *first* one we see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotTarget {
    /// One specific snapshot. `strain` is `Some` when the user spelled
    /// it out, `None` for a bare ID — the resolver looks it up across
    /// strains and errors if it is ambiguous.
    Single {
        strain: Option<String>,
        id: SnapshotId,
    },
    /// Every snapshot of a given strain.
    AllInStrain { strain: String },
}

impl SnapshotTarget {
    /// `true` for the bulk variant (`strain@` / `strain@all`).
    #[must_use]
    pub fn is_bulk(&self) -> bool {
        matches!(self, Self::AllInStrain { .. })
    }
}

impl fmt::Display for SnapshotTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Single {
                strain: Some(s),
                id,
            } => write!(f, "{s}@{id}"),
            Self::Single { strain: None, id } => write!(f, "{id}"),
            Self::AllInStrain { strain } => write!(f, "{strain}@"),
        }
    }
}

impl FromStr for SnapshotTarget {
    type Err = RevenantError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(RevenantError::Other(
                "empty snapshot target — expected ID, strain@ID, or strain@".to_string(),
            ));
        }
        match s.split_once('@') {
            Some((strain, rest)) => {
                if strain.is_empty() {
                    return Err(RevenantError::Other(format!(
                        "missing strain before '@' in {s:?}"
                    )));
                }
                if !crate::config::is_valid_strain_name(strain) {
                    return Err(RevenantError::Other(format!(
                        "invalid strain name {strain:?} in target — only [a-zA-Z0-9_] allowed"
                    )));
                }
                if rest.is_empty() || rest == "all" {
                    Ok(Self::AllInStrain {
                        strain: strain.to_string(),
                    })
                } else {
                    let id = SnapshotId::from_string(rest).map_err(|e| {
                        RevenantError::Other(format!("invalid snapshot ID in {s:?}: {e}"))
                    })?;
                    Ok(Self::Single {
                        strain: Some(strain.to_string()),
                        id,
                    })
                }
            }
            None => {
                let id = SnapshotId::from_string(s).map_err(|_| {
                    RevenantError::Other(format!("expected ID, strain@ID, or strain@ — got {s:?}"))
                })?;
                Ok(Self::Single { strain: None, id })
            }
        }
    }
}
