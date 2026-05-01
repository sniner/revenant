use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::Serialize;

use crate::config::RetainConfig;
use crate::snapshot::SnapshotInfo;

/// Reason a snapshot is selected for retention.  A snapshot may satisfy
/// several rules at once; `select_to_keep_explained` returns all of them.
///
/// `Protected` is the user-set sidecar flag and ranks above the
/// time-bucket rules: it always appears first in the per-snapshot reasons
/// list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeepReason {
    Protected,
    Last,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl KeepReason {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Protected => "protected",
            Self::Last => "last",
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
        }
    }
}

/// Given a list of snapshots and a retention config, return, for each kept
/// snapshot, the set of rules that protect it.  Snapshots absent from the
/// returned map are unprotected and may be deleted.
///
/// Reasons are returned in canonical rule order (`last`, `hourly`, `daily`,
/// `weekly`, `monthly`, `yearly`) so output is stable.
#[must_use]
pub fn select_to_keep_explained(
    snapshots: &[&SnapshotInfo],
    retain: &RetainConfig,
) -> HashMap<String, Vec<KeepReason>> {
    // Sort newest-first for restic-style bucket selection
    let mut sorted: Vec<&SnapshotInfo> = snapshots.to_vec();
    sorted.sort_by(|a, b| b.id.cmp(&a.id));

    let mut keep: HashMap<String, Vec<KeepReason>> = HashMap::new();

    // Protected ranks above the time-bucket rules so it lands first in
    // each snapshot's reason list. Iterating the original (unsorted) list
    // is fine — we are inserting into a map, not building an order.
    for snap in snapshots {
        if snap.metadata.as_ref().is_some_and(|m| m.protected) {
            keep.entry(snap.id.to_string())
                .or_default()
                .push(KeepReason::Protected);
        }
    }

    if retain.last > 0 {
        for snap in sorted.iter().take(retain.last) {
            keep.entry(snap.id.to_string())
                .or_default()
                .push(KeepReason::Last);
        }
    }

    if retain.hourly > 0 {
        mark_bucket(
            &sorted,
            retain.hourly,
            KeepReason::Hourly,
            &mut keep,
            |dt| {
                format!(
                    "{}{:02}{:02}{:02}",
                    dt.year(),
                    dt.month(),
                    dt.day(),
                    dt.hour()
                )
            },
        );
    }

    if retain.daily > 0 {
        mark_bucket(&sorted, retain.daily, KeepReason::Daily, &mut keep, |dt| {
            format!("{}{:02}{:02}", dt.year(), dt.month(), dt.day())
        });
    }

    if retain.weekly > 0 {
        mark_bucket(
            &sorted,
            retain.weekly,
            KeepReason::Weekly,
            &mut keep,
            |dt| {
                let w = dt.iso_week();
                format!("{}{:02}", w.year(), w.week())
            },
        );
    }

    if retain.monthly > 0 {
        mark_bucket(
            &sorted,
            retain.monthly,
            KeepReason::Monthly,
            &mut keep,
            |dt| format!("{}{:02}", dt.year(), dt.month()),
        );
    }

    if retain.yearly > 0 {
        mark_bucket(
            &sorted,
            retain.yearly,
            KeepReason::Yearly,
            &mut keep,
            |dt| format!("{}", dt.year()),
        );
    }

    keep
}

/// Given a list of snapshots and a retention config, returns the set of snapshot ID strings
/// that should be kept. Everything not in the returned set may be deleted.
#[must_use]
pub fn select_to_keep(snapshots: &[&SnapshotInfo], retain: &RetainConfig) -> HashSet<String> {
    select_to_keep_explained(snapshots, retain)
        .into_keys()
        .collect()
}

fn mark_bucket<F>(
    sorted: &[&SnapshotInfo],
    count: usize,
    reason: KeepReason,
    keep: &mut HashMap<String, Vec<KeepReason>>,
    key_fn: F,
) where
    F: Fn(&DateTime<Utc>) -> String,
{
    let mut seen_buckets = HashSet::new();

    for snap in sorted {
        if seen_buckets.len() >= count {
            break;
        }
        if let Some(dt) = snap.id.created_at() {
            let bucket = key_fn(&dt);
            if seen_buckets.insert(bucket) {
                keep.entry(snap.id.to_string()).or_default().push(reason);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{SnapshotMetadata, TriggerKind};
    use crate::snapshot::SnapshotId;

    fn make_snap(ts: &str) -> SnapshotInfo {
        SnapshotInfo {
            id: SnapshotId::from_string(ts).unwrap(),
            strain: "test".to_string(),
            subvolumes: vec!["@".to_string()],
            efi_synced: false,
            metadata: None,
        }
    }

    fn make_protected(ts: &str) -> SnapshotInfo {
        let mut s = make_snap(ts);
        s.metadata = Some(SnapshotMetadata::new(TriggerKind::Manual, vec![]).with_protected(true));
        s
    }

    #[test]
    fn last_only() {
        let snaps = [
            make_snap("20260101-000000"),
            make_snap("20260102-000000"),
            make_snap("20260103-000000"),
            make_snap("20260104-000000"),
            make_snap("20260105-000000"),
        ];
        let refs: Vec<&SnapshotInfo> = snaps.iter().collect();
        let retain = RetainConfig {
            last: 3,
            ..Default::default()
        };
        let keep = select_to_keep(&refs, &retain);
        assert_eq!(keep.len(), 3);
        assert!(keep.contains("20260103-000000"));
        assert!(keep.contains("20260104-000000"));
        assert!(keep.contains("20260105-000000"));
        assert!(!keep.contains("20260101-000000"));
        assert!(!keep.contains("20260102-000000"));
    }

    #[test]
    fn daily_deduplication() {
        // Multiple snapshots per day — only the newest per day is kept
        let snaps = [
            make_snap("20260101-000000"),
            make_snap("20260101-120000"),
            make_snap("20260102-000000"),
            make_snap("20260102-120000"),
            make_snap("20260103-000000"),
        ];
        let refs: Vec<&SnapshotInfo> = snaps.iter().collect();
        let retain = RetainConfig {
            last: 0,
            daily: 2,
            ..Default::default()
        };
        let keep = select_to_keep(&refs, &retain);
        assert_eq!(keep.len(), 2);
        assert!(keep.contains("20260103-000000"));
        assert!(keep.contains("20260102-120000")); // newest on that day
        assert!(!keep.contains("20260102-000000")); // older on same day
        assert!(!keep.contains("20260101-120000"));
        assert!(!keep.contains("20260101-000000"));
    }

    #[test]
    fn weekly_across_year_boundary() {
        // 2026-01-01 is a Thursday → ISO week 1 of 2026 is Mon 2025-12-29..Sun 2026-01-04
        // 2025-12-28 (Sunday) is in ISO week 52 of 2025
        let snaps = [
            make_snap("20251228-120000"), // 2025-W52
            make_snap("20251229-120000"), // 2026-W01
            make_snap("20260105-120000"), // 2026-W02
        ];
        let refs: Vec<&SnapshotInfo> = snaps.iter().collect();
        let retain = RetainConfig {
            last: 0,
            weekly: 2,
            ..Default::default()
        };
        let keep = select_to_keep(&refs, &retain);
        assert_eq!(keep.len(), 2);
        assert!(keep.contains("20260105-120000")); // W02-2026, newest
        assert!(keep.contains("20251229-120000")); // W01-2026
        assert!(!keep.contains("20251228-120000")); // W52-2025, 3rd distinct week
    }

    #[test]
    fn overlap_between_categories() {
        // A snapshot can satisfy multiple retention categories simultaneously
        let snaps = [
            make_snap("20260101-000000"),
            make_snap("20260201-000000"),
            make_snap("20260301-000000"),
        ];
        let refs: Vec<&SnapshotInfo> = snaps.iter().collect();
        let retain = RetainConfig {
            last: 1,
            monthly: 2,
            ..Default::default()
        };
        let keep = select_to_keep(&refs, &retain);
        // last=1 keeps 20260301, monthly=2 keeps 20260301 and 20260201
        assert_eq!(keep.len(), 2);
        assert!(keep.contains("20260301-000000"));
        assert!(keep.contains("20260201-000000"));
        assert!(!keep.contains("20260101-000000"));
    }

    #[test]
    fn explained_returns_all_matching_reasons() {
        // One snapshot can be protected by several rules simultaneously;
        // the explained variant returns every one in canonical rule order.
        let snaps = [
            make_snap("20260101-000000"),
            make_snap("20260201-000000"),
            make_snap("20260301-000000"),
        ];
        let refs: Vec<&SnapshotInfo> = snaps.iter().collect();
        let retain = RetainConfig {
            last: 1,
            monthly: 2,
            ..Default::default()
        };
        let keep = select_to_keep_explained(&refs, &retain);
        // Newest: last AND monthly; middle: monthly; oldest: nothing.
        assert_eq!(
            keep.get("20260301-000000"),
            Some(&vec![KeepReason::Last, KeepReason::Monthly])
        );
        assert_eq!(
            keep.get("20260201-000000"),
            Some(&vec![KeepReason::Monthly])
        );
        assert!(!keep.contains_key("20260101-000000"));
    }

    #[test]
    fn explained_matches_select_to_keep() {
        // The legacy API is a thin wrapper — the sets must agree.
        let snaps: Vec<_> = (1..=20)
            .map(|i| make_snap(&format!("2026{:02}01-000000", i % 12 + 1)))
            .collect();
        let refs: Vec<&SnapshotInfo> = snaps.iter().collect();
        let retain = RetainConfig {
            last: 3,
            monthly: 4,
            yearly: 1,
            ..Default::default()
        };
        let keep = select_to_keep(&refs, &retain);
        let explained = select_to_keep_explained(&refs, &retain);
        let explained_keys: HashSet<String> = explained.into_keys().collect();
        assert_eq!(keep, explained_keys);
    }

    #[test]
    fn protected_snapshot_is_kept_under_zero_retention() {
        // Even with all retention rules disabled, a protected snapshot
        // must end up in `keep` — only its companions get culled.
        let snaps = [
            make_snap("20260101-000000"),
            make_protected("20260102-000000"),
            make_snap("20260103-000000"),
        ];
        let refs: Vec<&SnapshotInfo> = snaps.iter().collect();
        let retain_zero = RetainConfig {
            last: 0,
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: 0,
            yearly: 0,
        };
        let keep = select_to_keep(&refs, &retain_zero);
        assert_eq!(keep.len(), 1);
        assert!(keep.contains("20260102-000000"));
    }

    #[test]
    fn protected_reason_appears_first() {
        // A snapshot that is both protected and last-bucket-eligible
        // must list `Protected` ahead of `Last` so callers see the
        // strongest reason first.
        let snaps = [
            make_protected("20260103-000000"),
            make_snap("20260102-000000"),
            make_snap("20260101-000000"),
        ];
        let refs: Vec<&SnapshotInfo> = snaps.iter().collect();
        let retain = RetainConfig {
            last: 1,
            ..Default::default()
        };
        let keep = select_to_keep_explained(&refs, &retain);
        assert_eq!(
            keep.get("20260103-000000"),
            Some(&vec![KeepReason::Protected, KeepReason::Last])
        );
    }

    #[test]
    fn all_zero_empty_keep() {
        let snaps = [make_snap("20260101-000000"), make_snap("20260102-000000")];
        let refs: Vec<&SnapshotInfo> = snaps.iter().collect();
        let retain = RetainConfig::default();
        // Construct explicitly all-zero (default() gives last=10, so override)
        let retain_zero = RetainConfig {
            last: 0,
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: 0,
            yearly: 0,
        };
        let keep = select_to_keep(&refs, &retain_zero);
        assert!(keep.is_empty());
        // Sanity: the real default keeps things
        let keep_default = select_to_keep(&refs, &retain);
        assert_eq!(keep_default.len(), 2);
    }
}
