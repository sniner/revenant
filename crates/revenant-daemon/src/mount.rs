//! Toplevel-mount lifecycle for the daemon.
//!
//! The daemon owns the btrfs toplevel mount for its entire runtime —
//! unlike the CLI, which mounts and umounts per command. Mount target
//! is `/run/revenant/toplevel`. The unit file (`PrivateMounts=true`)
//! keeps the mount inside the daemon's own namespace.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use revenant_core::Config;

/// Where the daemon mounts the btrfs toplevel. Lives on `tmpfs` (`/run`)
/// so it disappears across reboots; the daemon recreates it on start.
pub const TOPLEVEL_MOUNT_POINT: &str = "/run/revenant/toplevel";

/// RAII guard around the daemon's toplevel mount. Dropping it umounts
/// and removes the mount-point directory; failures are logged but don't
/// panic.
pub struct ToplevelMount {
    path: PathBuf,
}

impl ToplevelMount {
    /// Mount `subvolid=5` of the configured rootfs device at
    /// [`TOPLEVEL_MOUNT_POINT`]. Idempotent against a stale mount left
    /// over from a hard daemon kill.
    pub fn mount(config: &Config) -> Result<Self> {
        let mount_point = PathBuf::from(TOPLEVEL_MOUNT_POINT);
        // 0700 on both the parent and the mount point so non-root users
        // cannot traverse into the daemon's mount tree. Matters most for
        // dev runs without `PrivateMounts=true`; defense-in-depth under
        // systemd.
        if let Some(parent) = mount_point.parent() {
            ensure_private_dir(parent)?;
        }
        ensure_private_dir(&mount_point)?;

        // If a previous daemon instance was killed without running the
        // umount path, the mount-point will still be a mountpoint and a
        // fresh `mount` call would fail with EBUSY.
        if is_mount_point(&mount_point) {
            tracing::warn!(
                "stale mount at {} from a previous run; unmounting",
                mount_point.display()
            );
            if let Err(e) = nix::mount::umount(&mount_point) {
                tracing::warn!("failed to unmount stale {}: {e}", mount_point.display());
            }
        }

        let device = format!("/dev/disk/by-uuid/{}", config.sys.rootfs.device_uuid);

        nix::mount::mount(
            Some(device.as_str()),
            &mount_point,
            Some("btrfs"),
            nix::mount::MsFlags::empty(),
            Some("subvolid=5"),
        )
        .with_context(|| format!("mount {device} on {}", mount_point.display()))?;

        tracing::info!(
            "mounted btrfs toplevel ({}) on {}",
            device,
            mount_point.display()
        );

        Ok(Self { path: mount_point })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ToplevelMount {
    fn drop(&mut self) {
        if let Err(e) = nix::mount::umount(&self.path) {
            tracing::warn!("failed to unmount {}: {e}", self.path.display());
            return;
        }
        if let Err(e) = std::fs::remove_dir(&self.path) {
            tracing::debug!("failed to remove mount point {}: {e}", self.path.display());
        } else {
            tracing::info!("unmounted {} cleanly", self.path.display());
        }
    }
}

/// Create `path` (and any missing parents) with mode 0700, and re-apply
/// 0700 if the directory already existed — `DirBuilder::create` does not
/// touch the mode of pre-existing dirs, so we cannot rely on creation
/// alone to keep the mountpoint private across daemon restarts.
fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create dir {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0o700 {}", path.display()))?;
    Ok(())
}

/// Stat-based mount-point check: a path is a mount point iff its
/// device id differs from its parent's. Errors fall through to "not a
/// mount point" so a missing path retries the mount.
fn is_mount_point(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else {
        return false;
    };
    let (Ok(self_meta), Ok(parent_meta)) = (std::fs::metadata(path), std::fs::metadata(parent))
    else {
        return false;
    };
    self_meta.dev() != parent_meta.dev()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn ensure_private_dir_creates_new_with_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested/private");
        ensure_private_dir(&dir).unwrap();
        assert_eq!(mode_of(&dir), 0o700);
    }

    #[test]
    fn ensure_private_dir_tightens_existing_loose_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("loose");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(mode_of(&dir), 0o755);
        ensure_private_dir(&dir).unwrap();
        assert_eq!(mode_of(&dir), 0o700);
    }
}
