//! Raw Btrfs ioctl wrappers.
//!
//! These wrap the kernel's btrfs ioctl interface using `nix` and `libc`.
//! The kernel ABI is stable, so this is safe to use without libbtrfsutil.

use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;

use nix::errno::Errno;

use crate::error::{Result, RevenantError};

// Btrfs ioctl magic number
const BTRFS_IOCTL_MAGIC: u8 = 0x94;

// Ioctl command numbers
const BTRFS_IOC_SUBVOL_CREATE_NR: u8 = 14;
const BTRFS_IOC_SNAP_DESTROY_NR: u8 = 15;
const BTRFS_IOC_SNAP_CREATE_V2_NR: u8 = 23;
const BTRFS_IOC_SUBVOL_GETFLAGS_NR: u8 = 25;
const BTRFS_IOC_SUBVOL_SETFLAGS_NR: u8 = 26;
const BTRFS_IOC_GET_SUBVOL_INFO_NR: u8 = 60;
const BTRFS_IOC_DEFAULT_SUBVOL_NR: u8 = 19;

// Btrfs constants
pub const BTRFS_SUBVOL_RDONLY: u64 = 1 << 1;
const BTRFS_SUBVOL_NAME_MAX: usize = 4039;
const BTRFS_VOL_NAME_MAX: usize = 4087;

/// Arguments for `BTRFS_IOC_SNAP_CREATE_V2`.
#[repr(C)]
pub struct BtrfsIoctlVolArgsV2 {
    pub fd: i64,
    pub transid: u64,
    pub flags: u64,
    _unused: [u64; 4],
    pub name: [u8; BTRFS_SUBVOL_NAME_MAX + 1],
}

impl BtrfsIoctlVolArgsV2 {
    fn new(fd: i64, name: &[u8], flags: u64) -> Self {
        let mut args = Self {
            fd,
            transid: 0,
            flags,
            _unused: [0; 4],
            name: [0; BTRFS_SUBVOL_NAME_MAX + 1],
        };
        let len = name.len().min(BTRFS_SUBVOL_NAME_MAX);
        args.name[..len].copy_from_slice(&name[..len]);
        args
    }
}

/// Arguments for `BTRFS_IOC_SUBVOL_CREATE` / `BTRFS_IOC_SNAP_DESTROY`.
#[repr(C)]
pub struct BtrfsIoctlVolArgs {
    pub fd: i64,
    pub name: [u8; BTRFS_VOL_NAME_MAX + 1],
}

impl BtrfsIoctlVolArgs {
    fn new(name: &[u8]) -> Self {
        let mut args = Self {
            fd: 0,
            name: [0; BTRFS_VOL_NAME_MAX + 1],
        };
        let len = name.len().min(BTRFS_VOL_NAME_MAX);
        args.name[..len].copy_from_slice(&name[..len]);
        args
    }
}

/// Result from `BTRFS_IOC_GET_SUBVOL_INFO`.
#[repr(C)]
pub struct BtrfsIoctlGetSubvolInfoArgs {
    pub treeid: u64,
    pub name: [u8; 256],
    pub parent_id: u64,
    pub dirid: u64,
    pub generation: u64,
    pub flags: u64,
    pub uuid: [u8; 16],
    pub parent_uuid: [u8; 16],
    pub received_uuid: [u8; 16],
    pub ctransid: u64,
    pub otransid: u64,
    pub stransid: u64,
    pub rtransid: u64,
    pub ctime: BtrfsIoctlTimespec,
    pub otime: BtrfsIoctlTimespec,
    pub stime: BtrfsIoctlTimespec,
    pub rtime: BtrfsIoctlTimespec,
    _reserved: [u64; 8],
}

#[repr(C)]
pub struct BtrfsIoctlTimespec {
    pub sec: u64,
    pub nsec: u32,
}

impl Default for BtrfsIoctlGetSubvolInfoArgs {
    fn default() -> Self {
        // SAFETY: All fields are primitive integers or fixed-size arrays of primitives.
        // Zero is a valid bit pattern for every field in this #[repr(C)] struct.
        unsafe { std::mem::zeroed() }
    }
}

// Generate ioctl request codes using nix macros.
// These three are defined as _IOW in the kernel (btrfs.h), not _IOWR.
// Using ioctl_readwrite! would produce the wrong ioctl number → ENOTTY.
nix::ioctl_write_ptr!(
    btrfs_ioc_snap_create_v2,
    BTRFS_IOCTL_MAGIC,
    BTRFS_IOC_SNAP_CREATE_V2_NR,
    BtrfsIoctlVolArgsV2
);

nix::ioctl_write_ptr!(
    btrfs_ioc_subvol_create,
    BTRFS_IOCTL_MAGIC,
    BTRFS_IOC_SUBVOL_CREATE_NR,
    BtrfsIoctlVolArgs
);

nix::ioctl_write_ptr!(
    btrfs_ioc_snap_destroy,
    BTRFS_IOCTL_MAGIC,
    BTRFS_IOC_SNAP_DESTROY_NR,
    BtrfsIoctlVolArgs
);

nix::ioctl_read!(
    btrfs_ioc_subvol_getflags,
    BTRFS_IOCTL_MAGIC,
    BTRFS_IOC_SUBVOL_GETFLAGS_NR,
    u64
);

nix::ioctl_write_ptr!(
    btrfs_ioc_subvol_setflags,
    BTRFS_IOCTL_MAGIC,
    BTRFS_IOC_SUBVOL_SETFLAGS_NR,
    u64
);

nix::ioctl_read!(
    btrfs_ioc_get_subvol_info,
    BTRFS_IOCTL_MAGIC,
    BTRFS_IOC_GET_SUBVOL_INFO_NR,
    BtrfsIoctlGetSubvolInfoArgs
);

nix::ioctl_write_ptr!(
    btrfs_ioc_default_subvol,
    BTRFS_IOCTL_MAGIC,
    BTRFS_IOC_DEFAULT_SUBVOL_NR,
    u64
);

fn ioctl_err(path: &Path, msg: &str, errno: Errno) -> RevenantError {
    RevenantError::BtrfsIoctl {
        path: path.to_path_buf(),
        message: msg.to_string(),
        source: errno,
    }
}

/// Create a btrfs snapshot.
pub fn snap_create(
    parent_fd: BorrowedFd<'_>,
    source_fd: BorrowedFd<'_>,
    name: &str,
    readonly: bool,
    path_for_errors: &Path,
) -> Result<()> {
    let name_bytes = name.as_bytes();
    let flags = if readonly { BTRFS_SUBVOL_RDONLY } else { 0 };
    let args = BtrfsIoctlVolArgsV2::new(i64::from(source_fd.as_raw_fd()), name_bytes, flags);

    // SAFETY: args is fully initialized, parent_fd is a valid directory fd on a btrfs
    // filesystem, and the ioctl writes only within the bounds of BtrfsIoctlVolArgsV2.
    unsafe {
        btrfs_ioc_snap_create_v2(parent_fd.as_raw_fd(), &raw const args)
            .map_err(|e| ioctl_err(path_for_errors, "SNAP_CREATE_V2", e))?;
    }
    Ok(())
}

/// Create a btrfs subvolume.
pub fn subvol_create(parent_fd: BorrowedFd<'_>, name: &str, path_for_errors: &Path) -> Result<()> {
    let args = BtrfsIoctlVolArgs::new(name.as_bytes());

    // SAFETY: args is fully initialized and parent_fd is a valid directory fd on a btrfs
    // filesystem. The ioctl writes only within the bounds of BtrfsIoctlVolArgs.
    unsafe {
        btrfs_ioc_subvol_create(parent_fd.as_raw_fd(), &raw const args)
            .map_err(|e| ioctl_err(path_for_errors, "SUBVOL_CREATE", e))?;
    }
    Ok(())
}

/// Delete (destroy) a btrfs subvolume or snapshot.
pub fn snap_destroy(parent_fd: BorrowedFd<'_>, name: &str, path_for_errors: &Path) -> Result<()> {
    let args = BtrfsIoctlVolArgs::new(name.as_bytes());

    // SAFETY: args is fully initialized and parent_fd is a valid directory fd on a btrfs
    // filesystem. The ioctl writes only within the bounds of BtrfsIoctlVolArgs.
    unsafe {
        btrfs_ioc_snap_destroy(parent_fd.as_raw_fd(), &raw const args)
            .map_err(|e| ioctl_err(path_for_errors, "SNAP_DESTROY", e))?;
    }
    Ok(())
}

/// Get subvolume flags.
pub fn get_flags(fd: BorrowedFd<'_>, path_for_errors: &Path) -> Result<u64> {
    let mut flags: u64 = 0;
    // SAFETY: flags is a valid u64 and fd points to an open btrfs subvolume directory.
    // The ioctl writes exactly one u64.
    unsafe {
        btrfs_ioc_subvol_getflags(fd.as_raw_fd(), &raw mut flags)
            .map_err(|e| ioctl_err(path_for_errors, "SUBVOL_GETFLAGS", e))?;
    }
    Ok(flags)
}

/// Set subvolume flags.
pub fn set_flags(fd: BorrowedFd<'_>, flags: u64, path_for_errors: &Path) -> Result<()> {
    // SAFETY: flags is a valid u64 and fd points to an open btrfs subvolume directory.
    // The ioctl reads exactly one u64.
    unsafe {
        btrfs_ioc_subvol_setflags(fd.as_raw_fd(), &raw const flags)
            .map_err(|e| ioctl_err(path_for_errors, "SUBVOL_SETFLAGS", e))?;
    }
    Ok(())
}

/// Get subvolume info via ioctl.
pub fn get_subvol_info(
    fd: BorrowedFd<'_>,
    path_for_errors: &Path,
) -> Result<BtrfsIoctlGetSubvolInfoArgs> {
    let mut info = BtrfsIoctlGetSubvolInfoArgs::default();
    // SAFETY: info is zero-initialized and fd points to an open btrfs subvolume directory.
    // The ioctl writes only within the bounds of BtrfsIoctlGetSubvolInfoArgs.
    unsafe {
        btrfs_ioc_get_subvol_info(fd.as_raw_fd(), &raw mut info)
            .map_err(|e| ioctl_err(path_for_errors, "GET_SUBVOL_INFO", e))?;
    }
    Ok(info)
}

/// Set the default subvolume.
pub fn set_default_subvol(
    fd: BorrowedFd<'_>,
    subvol_id: u64,
    path_for_errors: &Path,
) -> Result<()> {
    // SAFETY: subvol_id is a valid u64 and fd points to an open btrfs filesystem.
    // The ioctl reads exactly one u64.
    unsafe {
        btrfs_ioc_default_subvol(fd.as_raw_fd(), &raw const subvol_id)
            .map_err(|e| ioctl_err(path_for_errors, "DEFAULT_SUBVOL", e))?;
    }
    Ok(())
}

/// Check if a path is on a btrfs filesystem by calling statfs.
pub fn is_btrfs(path: &Path) -> Result<bool> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| {
        RevenantError::Other(format!("path contains null byte: {}", path.display()))
    })?;
    // SAFETY: statfs_buf is all-zeros, which is valid for libc::statfs (all primitive fields).
    let mut statfs_buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid null-terminated C string and statfs_buf is a valid pointer.
    let ret = unsafe { libc::statfs(c_path.as_ptr(), &raw mut statfs_buf) };
    if ret != 0 {
        return Err(RevenantError::io(path, std::io::Error::last_os_error()));
    }
    // BTRFS_SUPER_MAGIC = 0x9123_683E
    Ok(statfs_buf.f_type == 0x9123_683E)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    // Kernel ABI checks: struct sizes must match the kernel definitions exactly.
    // The ioctl number encodes the struct size, so a mismatch produces ENOTTY
    // at runtime with no further diagnostics.

    #[test]
    fn vol_args_is_4096_bytes() {
        // Kernel: struct btrfs_ioctl_vol_args = { __s64 fd; char name[BTRFS_PATH_NAME_MAX+1]; }
        // BTRFS_PATH_NAME_MAX = 4087 → 8 + 4088 = 4096
        assert_eq!(size_of::<BtrfsIoctlVolArgs>(), 4096);
    }

    #[test]
    fn vol_args_v2_is_4096_bytes() {
        // Kernel: struct btrfs_ioctl_vol_args_v2 = { __s64 fd; __u64 transid; __u64 flags;
        //   union{...} [32 bytes]; char name[BTRFS_SUBVOL_NAME_MAX+1]; }
        // 8 + 8 + 8 + 32 + 4040 = 4096
        assert_eq!(size_of::<BtrfsIoctlVolArgsV2>(), 4096);
    }

    #[test]
    fn get_subvol_info_args_is_504_bytes() {
        // Kernel: struct btrfs_ioctl_get_subvol_info_args = 504 bytes
        // treeid(8) + name(256) + parent_id(8) + dirid(8) + generation(8) + flags(8)
        // + uuid(16) + parent_uuid(16) + received_uuid(16)
        // + ctransid(8) + otransid(8) + stransid(8) + rtransid(8)
        // + 4×timespec(16 each = 64) + reserved(64)
        assert_eq!(size_of::<BtrfsIoctlGetSubvolInfoArgs>(), 504);
    }

    #[test]
    fn timespec_is_16_bytes() {
        // Kernel: struct btrfs_ioctl_timespec = { __u64 sec; __u32 nsec; }
        // With repr(C) padding after nsec: 16 bytes
        assert_eq!(size_of::<BtrfsIoctlTimespec>(), 16);
    }
}
