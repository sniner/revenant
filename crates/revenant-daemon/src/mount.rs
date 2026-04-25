//! Toplevel-mount lifecycle for the daemon.
//!
//! The daemon owns the btrfs toplevel mount for its entire runtime —
//! unlike the CLI, which mounts and umounts per command. Mount target
//! is `/run/revenant/toplevel`. The unit file (`PrivateMounts=true`)
//! keeps the mount inside the daemon's own namespace.

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
        if let Some(parent) = mount_point.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir {}", parent.display()))?;
        }
        std::fs::create_dir_all(&mount_point)
            .with_context(|| format!("create mount point {}", mount_point.display()))?;

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
