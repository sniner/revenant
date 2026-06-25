//! Raw Btrfs ioctl wrappers.
//!
//! These wrap the kernel's btrfs ioctl interface using `nix` and `libc`.
//! The kernel ABI is stable, so this is safe to use without libbtrfsutil.

use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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
const BTRFS_IOC_TREE_SEARCH_NR: u8 = 17;
const BTRFS_IOC_INO_LOOKUP_NR: u8 = 18;

// Btrfs constants
pub const BTRFS_SUBVOL_RDONLY: u64 = 1 << 1;
const BTRFS_SUBVOL_NAME_MAX: usize = 4039;
const BTRFS_VOL_NAME_MAX: usize = 4087;

// Tree/key identifiers used for nested-subvolume enumeration.
const BTRFS_ROOT_TREE_OBJECTID: u64 = 1;
const BTRFS_ROOT_REF_KEY: u32 = 156;

// Layout of `struct btrfs_ioctl_search_args`: a 104-byte key followed by a
// data buffer that brings the whole struct up to 4096 bytes.
const BTRFS_SEARCH_ARGS_BUFSIZE: usize = 4096 - 104;
// `struct btrfs_ioctl_ino_lookup_args`: two u64s plus a 4080-byte name.
const BTRFS_INO_LOOKUP_PATH_MAX: usize = 4080;
// On-disk `struct btrfs_ioctl_search_header` preceding every result item.
const SEARCH_HEADER_SIZE: usize = 32;

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

/// Search key for `BTRFS_IOC_TREE_SEARCH` — mirrors
/// `struct btrfs_ioctl_search_key` (104 bytes). Most fields are written for
/// the kernel and never read back by Rust (only `nr_items` is), so the
/// struct as a whole opts out of the dead-code lint.
#[repr(C)]
#[allow(dead_code)]
struct BtrfsIoctlSearchKey {
    tree_id: u64,
    min_objectid: u64,
    max_objectid: u64,
    min_offset: u64,
    max_offset: u64,
    min_transid: u64,
    max_transid: u64,
    min_type: u32,
    max_type: u32,
    /// In: max number of items to return. Out: number actually returned.
    nr_items: u32,
    unused: u32,
    unused1: u64,
    unused2: u64,
    unused3: u64,
    unused4: u64,
}

/// Arguments for `BTRFS_IOC_TREE_SEARCH` — `struct btrfs_ioctl_search_args`
/// (4096 bytes total: the key plus a result buffer).
#[repr(C)]
struct BtrfsIoctlSearchArgs {
    key: BtrfsIoctlSearchKey,
    buf: [u8; BTRFS_SEARCH_ARGS_BUFSIZE],
}

impl Default for BtrfsIoctlSearchArgs {
    fn default() -> Self {
        // SAFETY: every field is an integer or a byte array; all-zero is a
        // valid bit pattern for this #[repr(C)] struct.
        unsafe { std::mem::zeroed() }
    }
}

/// Arguments for `BTRFS_IOC_INO_LOOKUP` — `struct btrfs_ioctl_ino_lookup_args`
/// (4096 bytes). Resolves an inode number within a subvolume tree to its
/// path from the subvolume root. `treeid`/`objectid` are written for the
/// kernel and not read back, so the struct opts out of the dead-code lint.
#[repr(C)]
#[allow(dead_code)]
struct BtrfsIoctlInoLookupArgs {
    treeid: u64,
    objectid: u64,
    name: [u8; BTRFS_INO_LOOKUP_PATH_MAX],
}

impl Default for BtrfsIoctlInoLookupArgs {
    fn default() -> Self {
        // SAFETY: integers plus a byte array; all-zero is valid.
        unsafe { std::mem::zeroed() }
    }
}

/// One `BTRFS_ROOT_REF_KEY` item parsed out of a tree-search result: a
/// direct child subvolume of the searched subvolume.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RootRef {
    /// Subvolume id of the child (the search key's `offset`).
    child_id: u64,
    /// Inode of the directory *inside the parent subvolume* that holds the
    /// child's directory entry. Resolved to a path via `INO_LOOKUP`.
    dirid: u64,
    /// The child subvolume's directory-entry name within `dirid`.
    name: Vec<u8>,
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

// TREE_SEARCH and INO_LOOKUP are _IOWR in the kernel (they both read the
// request and write results back into the same buffer).
nix::ioctl_readwrite!(
    btrfs_ioc_tree_search,
    BTRFS_IOCTL_MAGIC,
    BTRFS_IOC_TREE_SEARCH_NR,
    BtrfsIoctlSearchArgs
);

nix::ioctl_readwrite!(
    btrfs_ioc_ino_lookup,
    BTRFS_IOCTL_MAGIC,
    BTRFS_IOC_INO_LOOKUP_NR,
    BtrfsIoctlInoLookupArgs
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

/// Read a little-endian integer out of a byte slice at `off`, or `None`
/// if the slice is too short. Btrfs on-disk metadata is little-endian.
fn read_u64_le(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}
fn read_u32_le(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}
fn read_u16_le(b: &[u8], off: usize) -> Option<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
}

/// Parse `nr_items` results out of a `BTRFS_IOC_TREE_SEARCH` result buffer,
/// keeping only the `ROOT_REF` items.
///
/// Each result is a 32-byte `btrfs_ioctl_search_header`
/// (transid, objectid, offset, type, len) followed by `len` bytes of item
/// data. For a ROOT_REF the header's `offset` is the child subvolume id and
/// the data is `struct btrfs_root_ref` (dirid, sequence, name_len, name).
///
/// Pure function so the fiddly offset arithmetic can be unit-tested without
/// a real filesystem.
fn parse_root_refs(buf: &[u8], nr_items: u32) -> Vec<RootRef> {
    let mut out = Vec::new();
    let mut off = 0usize;
    for _ in 0..nr_items {
        // Header: transid(0) objectid(8) offset(16) type(24) len(28).
        let (Some(child_id), Some(item_type), Some(item_len)) = (
            read_u64_le(buf, off + 16),
            read_u32_le(buf, off + 24),
            read_u32_le(buf, off + 28),
        ) else {
            break;
        };
        let data_start = off + SEARCH_HEADER_SIZE;
        let Some(data_end) = data_start.checked_add(item_len as usize) else {
            break;
        };
        if data_end > buf.len() {
            break;
        }
        if item_type == BTRFS_ROOT_REF_KEY {
            let data = &buf[data_start..data_end];
            // btrfs_root_ref: dirid(0) sequence(8) name_len(16) name(18..).
            if let (Some(dirid), Some(name_len)) = (read_u64_le(data, 0), read_u16_le(data, 16)) {
                let name_end = (18 + name_len as usize).min(data.len());
                let name = data.get(18..name_end).unwrap_or(&[]).to_vec();
                out.push(RootRef {
                    child_id,
                    dirid,
                    name,
                });
            }
        }
        off = data_end;
    }
    out
}

/// Extract the NUL-terminated path out of an `INO_LOOKUP` `name` buffer.
fn ino_path_bytes(name: &[u8]) -> &[u8] {
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    &name[..end]
}

/// Join an `INO_LOOKUP` directory path (e.g. `b"var/lib/"`, empty at the
/// subvolume root) with a child subvolume name into a path relative to the
/// subvolume root.
fn assemble_rel_path(dir: &[u8], name: &[u8]) -> PathBuf {
    let mut bytes = Vec::with_capacity(dir.len() + 1 + name.len());
    bytes.extend_from_slice(dir);
    // INO_LOOKUP returns a trailing slash for non-empty paths, but guard
    // against a kernel that omits it so we never fuse two components.
    if !dir.is_empty() && dir.last() != Some(&b'/') {
        bytes.push(b'/');
    }
    bytes.extend_from_slice(name);
    PathBuf::from(OsStr::from_bytes(&bytes))
}

/// Resolve an inode number within `treeid` to its path from the subvolume
/// root via `BTRFS_IOC_INO_LOOKUP`. Returns the raw path bytes (with a
/// trailing slash; empty for the root inode itself).
fn ino_lookup(
    fd: BorrowedFd<'_>,
    treeid: u64,
    objectid: u64,
    path_for_errors: &Path,
) -> Result<Vec<u8>> {
    let mut args = BtrfsIoctlInoLookupArgs {
        treeid,
        objectid,
        ..Default::default()
    };
    // SAFETY: args is fully initialized and fd is an open btrfs fd. The ioctl
    // writes only within the bounds of BtrfsIoctlInoLookupArgs.
    unsafe {
        btrfs_ioc_ino_lookup(fd.as_raw_fd(), &raw mut args)
            .map_err(|e| ioctl_err(path_for_errors, "INO_LOOKUP", e))?;
    }
    Ok(ino_path_bytes(&args.name).to_vec())
}

/// Enumerate the *direct* nested subvolumes of the subvolume identified by
/// `subvol_id`, returning their paths relative to that subvolume's root
/// (e.g. `var/lib/docker`).
///
/// Reads the root tree's `ROOT_REF` items — the same mechanism
/// `btrfs subvolume list` uses — instead of walking the directory tree, so
/// the cost is O(nested subvolumes) rather than O(directories). `fd` may be
/// any open fd on the same filesystem.
pub fn nested_subvol_rel_paths(
    fd: BorrowedFd<'_>,
    subvol_id: u64,
    path_for_errors: &Path,
) -> Result<Vec<PathBuf>> {
    let mut refs: Vec<RootRef> = Vec::new();
    // ROOT_REFs for a parent are keyed (objectid=parent, type=ROOT_REF,
    // offset=child). Pin objectid and type, page through `offset` until the
    // search comes back empty.
    let mut min_offset = 0u64;
    loop {
        let mut args = BtrfsIoctlSearchArgs {
            key: BtrfsIoctlSearchKey {
                tree_id: BTRFS_ROOT_TREE_OBJECTID,
                min_objectid: subvol_id,
                max_objectid: subvol_id,
                min_offset,
                max_offset: u64::MAX,
                min_transid: 0,
                max_transid: u64::MAX,
                min_type: BTRFS_ROOT_REF_KEY,
                max_type: BTRFS_ROOT_REF_KEY,
                nr_items: u32::MAX,
                unused: 0,
                unused1: 0,
                unused2: 0,
                unused3: 0,
                unused4: 0,
            },
            buf: [0; BTRFS_SEARCH_ARGS_BUFSIZE],
        };
        // SAFETY: args is fully initialized and fd is an open btrfs fd. The
        // ioctl reads the key and writes results within the struct bounds.
        unsafe {
            btrfs_ioc_tree_search(fd.as_raw_fd(), &raw mut args)
                .map_err(|e| ioctl_err(path_for_errors, "TREE_SEARCH", e))?;
        }
        if args.key.nr_items == 0 {
            break;
        }
        let batch = parse_root_refs(&args.buf, args.key.nr_items);
        let Some(max_child) = batch.iter().map(|r| r.child_id).max() else {
            break;
        };
        refs.extend(batch);
        if max_child == u64::MAX {
            break;
        }
        min_offset = max_child + 1;
    }

    let mut out = Vec::with_capacity(refs.len());
    for r in refs {
        let dir = ino_lookup(fd, subvol_id, r.dirid, path_for_errors)?;
        out.push(assemble_rel_path(&dir, &r.name));
    }
    Ok(out)
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

    #[test]
    fn search_key_is_104_bytes() {
        // Kernel: struct btrfs_ioctl_search_key
        // 7×u64 (tree_id..max_transid) = 56, 4×u32 (min_type, max_type,
        // nr_items, unused) = 16, 4×u64 (unused1..4) = 32 → 104.
        assert_eq!(size_of::<BtrfsIoctlSearchKey>(), 104);
    }

    #[test]
    fn search_args_is_4096_bytes() {
        // Kernel: struct btrfs_ioctl_search_args = key(104) + buf(3992).
        assert_eq!(size_of::<BtrfsIoctlSearchArgs>(), 4096);
    }

    #[test]
    fn ino_lookup_args_is_4096_bytes() {
        // Kernel: struct btrfs_ioctl_ino_lookup_args = treeid(8) + objectid(8)
        // + name(4080) = 4096.
        assert_eq!(size_of::<BtrfsIoctlInoLookupArgs>(), 4096);
    }

    /// Build a single tree-search result item: a 32-byte header followed by
    /// the item payload. Mirrors the on-disk little-endian layout the kernel
    /// writes into the search buffer.
    fn search_item(objectid: u64, offset: u64, item_type: u32, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0u64.to_le_bytes()); // transid
        v.extend_from_slice(&objectid.to_le_bytes()); // objectid (parent)
        v.extend_from_slice(&offset.to_le_bytes()); // offset (child id)
        v.extend_from_slice(&item_type.to_le_bytes()); // type
        v.extend_from_slice(&(data.len() as u32).to_le_bytes()); // len
        v.extend_from_slice(data);
        v
    }

    /// Build a `btrfs_root_ref` item payload: dirid, sequence, name_len, name.
    fn root_ref_data(dirid: u64, name: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&dirid.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes()); // sequence
        v.extend_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(name);
        v
    }

    #[test]
    fn parse_root_refs_extracts_children() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&search_item(
            256,
            300,
            BTRFS_ROOT_REF_KEY,
            &root_ref_data(257, b"docker"),
        ));
        buf.extend_from_slice(&search_item(
            256,
            301,
            BTRFS_ROOT_REF_KEY,
            &root_ref_data(256, b"machines"),
        ));

        let refs = parse_root_refs(&buf, 2);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].child_id, 300);
        assert_eq!(refs[0].dirid, 257);
        assert_eq!(refs[0].name, b"docker");
        assert_eq!(refs[1].child_id, 301);
        assert_eq!(refs[1].dirid, 256);
        assert_eq!(refs[1].name, b"machines");
    }

    #[test]
    fn parse_root_refs_skips_non_rootref_items() {
        let mut buf = Vec::new();
        // A ROOT_ITEM (132) we must ignore, then a ROOT_REF we keep.
        buf.extend_from_slice(&search_item(256, 999, 132, &[0u8; 40]));
        buf.extend_from_slice(&search_item(
            256,
            300,
            BTRFS_ROOT_REF_KEY,
            &root_ref_data(257, b"portables"),
        ));
        let refs = parse_root_refs(&buf, 2);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, b"portables");
    }

    #[test]
    fn parse_root_refs_stops_on_truncated_buffer() {
        // Claim two items but only supply one full item's worth of bytes.
        let buf = search_item(256, 300, BTRFS_ROOT_REF_KEY, &root_ref_data(257, b"x"));
        let refs = parse_root_refs(&buf, 5);
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn ino_path_bytes_truncates_at_nul() {
        assert_eq!(ino_path_bytes(b"var/lib/\0\0\0"), b"var/lib/");
        assert_eq!(ino_path_bytes(b""), b"");
        assert_eq!(ino_path_bytes(b"\0"), b"");
    }

    #[test]
    fn assemble_rel_path_joins_dir_and_name() {
        assert_eq!(
            assemble_rel_path(b"var/lib/", b"docker"),
            PathBuf::from("var/lib/docker")
        );
        // Empty dir → the child sits at the subvolume root.
        assert_eq!(assemble_rel_path(b"", b"docker"), PathBuf::from("docker"));
        // Defensive: missing trailing slash must not fuse components.
        assert_eq!(
            assemble_rel_path(b"var/lib", b"docker"),
            PathBuf::from("var/lib/docker")
        );
    }
}
