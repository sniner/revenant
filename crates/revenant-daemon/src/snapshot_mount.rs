//! Read-only snapshot mounts for the GUI.
//!
//! The GUI offers a "browse this snapshot" action that mounts every
//! subvolume of a snapshot read-only under
//! `/run/user/<uid>/revenant/mounts/<strain>-<id>/<subvol>/` and opens
//! the user's file manager on it. The CLI deliberately does not use
//! this — `mount -o subvol=…,ro` from a shell is just as fast for
//! someone who is already in a terminal.
//!
//! Lifecycle:
//! * `mount_snapshot` — creates the per-snapshot mount tree and
//!   mounts each subvolume `ro,nodev,nosuid`. Idempotent against an
//!   already-mounted snapshot: returns the existing paths and resets
//!   the idle clock.
//! * `unmount_snapshot` — unmounts every subvolume of the snapshot
//!   and removes the tree. Idempotent for snapshots that aren't
//!   currently mounted.
//! * `idle_sweep` — periodic cleanup. EBUSY (file manager still has
//!   it open) keeps the entry and resets the idle clock; everything
//!   else is unmounted and dropped.
//! * `recover_stale_at_startup` — runs once on daemon start to
//!   reclaim mounts left behind by a previous instance that was
//!   killed without the chance to umount.
//! * `shutdown` — best-effort umount of every active mount on
//!   daemon stop.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use nix::unistd::{Gid, Uid, User};
use revenant_core::Config;
use revenant_core::snapshot::SnapshotInfo;

const RUN_USER_DIR: &str = "/run/user";
/// Path fragment under `/run/user/<uid>/` that holds revenant's
/// per-snapshot mount trees. Mirrored verbatim by the stale-recovery
/// scan, so changing it requires a coordinated update there.
const MOUNT_SUBPATH: &str = "revenant/mounts";

/// How long a mount may sit untouched before `idle_sweep` tries to
/// unmount it. EBUSY (file manager still has it open) resets the
/// clock; otherwise the mount is reclaimed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// One snapshot's active mount, tracked by `(strain, id)`.
#[derive(Debug)]
struct ActiveMount {
    /// `/run/user/<uid>/revenant/mounts/<strain>-<id>/`.
    base_dir: PathBuf,
    /// `(subvol_name, mount_point)` per subvolume of the snapshot.
    subvols: Vec<(String, PathBuf)>,
    /// Updated on every `mount_snapshot` call (including the
    /// idempotent re-mount path) so an active GUI session keeps the
    /// mount alive across the idle sweep.
    last_active: Instant,
}

#[derive(Default)]
pub struct SnapshotMountManager {
    inner: Mutex<HashMap<(String, String), ActiveMount>>,
}

impl SnapshotMountManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount every subvolume of `snapshot` read-only and return a map
    /// `subvol_name -> mount_path`. Idempotent: a second call for a
    /// snapshot that's already mounted just refreshes `last_active`
    /// and returns the existing paths.
    ///
    /// `uid`/`gid` set the ownership of the per-user mount tree so
    /// the calling user can traverse into it. The mounted contents
    /// retain the snapshot's original permissions.
    pub fn mount_snapshot(
        &self,
        config: &Config,
        snapshot: &SnapshotInfo,
        uid: u32,
        gid: u32,
    ) -> Result<HashMap<String, String>> {
        let key = (snapshot.strain.clone(), snapshot.id.to_string());
        let mut map = self.inner.lock().expect("snapshot_mount lock poisoned");

        if let Some(existing) = map.get_mut(&key) {
            existing.last_active = Instant::now();
            return Ok(paths_to_map(&existing.subvols));
        }

        let base_dir = base_dir_for(uid, &snapshot.strain, snapshot.id.as_str());
        ensure_owned_chain(&base_dir, uid, gid)?;

        let device = format!("/dev/disk/by-uuid/{}", config.sys.rootfs.device_uuid);
        let mut mounted: Vec<(String, PathBuf)> = Vec::with_capacity(snapshot.subvolumes.len());

        // ro / nodev / nosuid are mount flags, *not* btrfs data
        // options — passing them in the data string makes btrfs
        // reject the mount with EINVAL. The actual btrfs option is
        // just `subvol=`.
        let flags = nix::mount::MsFlags::MS_RDONLY
            | nix::mount::MsFlags::MS_NODEV
            | nix::mount::MsFlags::MS_NOSUID;
        let outcome = (|| -> Result<()> {
            for sv in &snapshot.subvolumes {
                let snap_subvol = format!(
                    "{}/{}",
                    config.sys.snapshot_subvol,
                    snapshot.id.snapshot_name(sv, &snapshot.strain),
                );
                let mount_point = base_dir.join(sv);
                ensure_owned_dir(&mount_point, uid, gid)?;
                let data = format!("subvol={snap_subvol}");
                nix::mount::mount(
                    Some(device.as_str()),
                    &mount_point,
                    Some("btrfs"),
                    flags,
                    Some(data.as_str()),
                )
                .with_context(|| format!("mount {} on {}", snap_subvol, mount_point.display()))?;
                tracing::info!(
                    "mounted snapshot subvol {} ro on {}",
                    snap_subvol,
                    mount_point.display(),
                );
                mounted.push((sv.clone(), mount_point));
            }
            Ok(())
        })();

        if let Err(e) = outcome {
            for (_, mp) in &mounted {
                if let Err(ue) = nix::mount::umount(mp) {
                    tracing::warn!("rollback umount {}: {ue}", mp.display());
                }
            }
            let _ = fs::remove_dir_all(&base_dir);
            return Err(e);
        }

        let response = paths_to_map(&mounted);
        map.insert(
            key,
            ActiveMount {
                base_dir,
                subvols: mounted,
                last_active: Instant::now(),
            },
        );
        Ok(response)
    }

    /// Unmount every subvolume of the snapshot and drop the entry.
    /// Idempotent: succeeds with no-op when the snapshot isn't
    /// currently mounted by us.
    pub fn unmount_snapshot(&self, strain: &str, id: &str) -> Result<()> {
        let key = (strain.to_string(), id.to_string());
        let mut map = self.inner.lock().expect("snapshot_mount lock poisoned");
        let Some(mount) = map.remove(&key) else {
            return Ok(());
        };
        unmount_active(&mount)
    }

    /// Reclaim mounts that have been idle past [`IDLE_TIMEOUT`].
    /// EBUSY ⇒ the mount is still in use (file manager open), reset
    /// the clock and try again next round. Other umount failures are
    /// logged but the entry is dropped — the kernel state and our
    /// view diverged, which is best-handled by giving up tracking.
    pub fn idle_sweep(&self) {
        let now = Instant::now();
        let mut map = self.inner.lock().expect("snapshot_mount lock poisoned");
        let mut to_drop: Vec<(String, String)> = Vec::new();
        for (key, mount) in map.iter_mut() {
            if now.saturating_duration_since(mount.last_active) < IDLE_TIMEOUT {
                continue;
            }
            let mut busy = false;
            let mut other_failure = false;
            for (_, mp) in &mount.subvols {
                match nix::mount::umount(mp) {
                    Ok(()) => {}
                    Err(nix::errno::Errno::EBUSY) => busy = true,
                    Err(e) => {
                        tracing::warn!("idle umount {}: {e}", mp.display());
                        other_failure = true;
                    }
                }
            }
            if busy {
                mount.last_active = Instant::now();
            } else {
                if other_failure {
                    tracing::warn!(
                        "idle sweep dropped tracking for {}@{} despite umount failures",
                        key.0,
                        key.1,
                    );
                }
                let _ = fs::remove_dir_all(&mount.base_dir);
                to_drop.push(key.clone());
                tracing::info!("idle-unmounted snapshot {}@{}", key.0, key.1);
            }
        }
        for key in to_drop {
            map.remove(&key);
        }
    }

    /// Best-effort umount of every active mount, used on daemon shutdown.
    pub fn shutdown(&self) {
        let mut map = self.inner.lock().expect("snapshot_mount lock poisoned");
        for ((strain, id), mount) in map.drain() {
            if let Err(e) = unmount_active(&mount) {
                tracing::warn!("shutdown umount {strain}@{id}: {e}");
            }
        }
    }
}

/// Resolve the primary GID of a UNIX user so per-user mount-tree
/// directories can be `chown`ed to (uid, primary_gid). Falls back to
/// the uid as gid if the lookup fails — same fallback pattern as
/// `useradd` private groups.
pub fn resolve_primary_gid(uid: u32) -> u32 {
    match User::from_uid(Uid::from_raw(uid)) {
        Ok(Some(user)) => user.gid.as_raw(),
        _ => uid,
    }
}

/// Sweep every leftover mount under `/run/user/*/revenant/mounts/`
/// — used at daemon start to reclaim mounts a previous instance left
/// behind. Reads `/proc/self/mountinfo` to find current mounts under
/// our path prefix, umounts each, then rmdirs the empty trees.
pub fn recover_stale_at_startup() {
    let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
        return;
    };

    let mut umounted = 0usize;
    for line in mountinfo.lines() {
        // mountinfo fields are space-separated; field 5 (0-indexed 4)
        // is the mountpoint. Spaces in the path are encoded as octal,
        // but our mountpoints never contain spaces.
        let Some(mp) = line.split_whitespace().nth(4) else {
            continue;
        };
        if !is_revenant_user_mount_path(mp) {
            continue;
        }
        match nix::mount::umount(Path::new(mp)) {
            Ok(()) => {
                umounted += 1;
                tracing::warn!("reclaimed stale snapshot mount at {mp}");
            }
            Err(e) => tracing::warn!("could not reclaim stale mount {mp}: {e}"),
        }
    }

    if let Ok(entries) = fs::read_dir(RUN_USER_DIR) {
        for ent in entries.flatten() {
            let dir = ent.path().join(MOUNT_SUBPATH);
            if dir.is_dir() {
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }

    if umounted > 0 {
        tracing::info!("startup: cleared {umounted} stale snapshot mount(s)");
    }
}

fn is_revenant_user_mount_path(p: &str) -> bool {
    // /run/user/<uid>/revenant/mounts/<strain>-<id>/<subvol>
    let Some(rest) = p.strip_prefix("/run/user/") else {
        return false;
    };
    let Some((_uid, tail)) = rest.split_once('/') else {
        return false;
    };
    tail.starts_with("revenant/mounts/")
}

fn unmount_active(mount: &ActiveMount) -> Result<()> {
    let mut last_err: Option<anyhow::Error> = None;
    for (_, mp) in &mount.subvols {
        match nix::mount::umount(mp) {
            Ok(()) => {}
            // EINVAL ≈ "not a mount point any more" — already gone, treat as success.
            Err(nix::errno::Errno::EINVAL) => {}
            Err(e) => {
                tracing::warn!("umount {}: {e}", mp.display());
                last_err = Some(anyhow!("umount {}: {e}", mp.display()));
            }
        }
    }
    let _ = fs::remove_dir_all(&mount.base_dir);
    last_err.map_or(Ok(()), Err)
}

fn base_dir_for(uid: u32, strain: &str, id: &str) -> PathBuf {
    PathBuf::from(format!(
        "{RUN_USER_DIR}/{uid}/{MOUNT_SUBPATH}/{strain}-{id}"
    ))
}

fn paths_to_map(subvols: &[(String, PathBuf)]) -> HashMap<String, String> {
    subvols
        .iter()
        .map(|(name, path)| (name.clone(), path.to_string_lossy().into_owned()))
        .collect()
}

/// Walk each new path component beneath `/run/user/<uid>/` down to
/// `leaf` and apply [`ensure_owned_dir`] to it. Necessary because
/// otherwise the intermediate dirs (`revenant/`, `revenant/mounts/`)
/// stay root-owned with the leaf's mode 0700, blocking the user from
/// traversing into their own mount tree.
///
/// `/run/user/<uid>/` itself is logind's territory and is never
/// touched.
fn ensure_owned_chain(leaf: &Path, uid: u32, gid: u32) -> Result<()> {
    let user_root = PathBuf::from(format!("{RUN_USER_DIR}/{uid}"));
    let stripped = leaf
        .strip_prefix(&user_root)
        .with_context(|| format!("{} not under {}", leaf.display(), user_root.display()))?;
    let mut cur = user_root.clone();
    for component in stripped.iter() {
        cur.push(component);
        ensure_owned_dir(&cur, uid, gid)?;
    }
    Ok(())
}

/// Create `path` (mode 0700) and chown to (uid, gid) so the calling
/// user can traverse into it. Re-applies mode and ownership on each
/// call so leftover state from a previous run is normalized.
fn ensure_owned_dir(path: &Path, uid: u32, gid: u32) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create dir {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0o700 {}", path.display()))?;
    nix::unistd::chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
        .with_context(|| format!("chown {} -> {uid}:{gid}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_path_layout() {
        let p = base_dir_for(1000, "default", "20260316-143022-456");
        assert_eq!(
            p,
            PathBuf::from("/run/user/1000/revenant/mounts/default-20260316-143022-456"),
        );
    }

    #[test]
    fn revenant_user_mount_path_recognises_prefix() {
        assert!(is_revenant_user_mount_path(
            "/run/user/1000/revenant/mounts/default-20260316-143022-456/@"
        ));
        assert!(is_revenant_user_mount_path(
            "/run/user/0/revenant/mounts/anything"
        ));
    }

    #[test]
    fn revenant_user_mount_path_rejects_others() {
        assert!(!is_revenant_user_mount_path("/run/user/1000"));
        assert!(!is_revenant_user_mount_path("/run/user/1000/foo"));
        assert!(!is_revenant_user_mount_path("/run/revenant/toplevel"));
        assert!(!is_revenant_user_mount_path("/tmp"));
        assert!(!is_revenant_user_mount_path(
            "/run/user/1000/revenant/other"
        ));
    }
}
