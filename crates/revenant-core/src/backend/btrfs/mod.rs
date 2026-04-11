pub mod ioctl;

use std::fs::{self, File};
use std::os::fd::AsFd;
use std::path::Path;

use uuid::Uuid;

use super::{FileSystemBackend, SubvolumeInfo};
use crate::error::{Result, RevenantError};

/// Btrfs filesystem backend.
pub struct BtrfsBackend;

impl Default for BtrfsBackend {
    fn default() -> Self {
        Self
    }
}

impl BtrfsBackend {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Open a directory fd for ioctl operations.
    fn open_dir(path: &Path) -> Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| RevenantError::io(path, e))
    }

    /// Extract parent directory and filename from a path.
    fn parent_and_name(path: &Path) -> Result<(&Path, &str)> {
        let parent = path
            .parent()
            .ok_or_else(|| RevenantError::Other(format!("no parent for {}", path.display())))?;
        let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            RevenantError::Other(format!("invalid filename in {}", path.display()))
        })?;
        Ok((parent, name))
    }
}

impl BtrfsBackend {
    fn find_nested_recursive(
        &self,
        dir: &Path,
        result: &mut Vec<std::path::PathBuf>,
    ) -> Result<()> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if self.subvolume_info(&path).is_ok() {
                result.push(path);
                // Don't recurse into nested subvolumes — they'll handle
                // their own children when delete_subvolume is called on them.
            } else {
                // Regular directory — recurse to find deeper nested subvolumes
                self.find_nested_recursive(&path, result)?;
            }
        }
        Ok(())
    }
}

impl FileSystemBackend for BtrfsBackend {
    fn probe(&self, path: &Path) -> Result<bool> {
        ioctl::is_btrfs(path)
    }

    fn list_subvolumes(&self, root: &Path) -> Result<Vec<SubvolumeInfo>> {
        // Walk directory entries and probe each for subvolume info.
        // This is a simplified implementation — a production version would use
        // BTRFS_IOC_TREE_SEARCH for efficiency.
        let mut subvols = Vec::new();
        let entries = fs::read_dir(root).map_err(|e| RevenantError::io(root, e))?;

        for entry in entries {
            let entry = entry.map_err(|e| RevenantError::io(root, e))?;
            let path = entry.path();
            if path.is_dir() {
                if let Ok(info) = self.subvolume_info(&path) {
                    subvols.push(info);
                }
            }
        }

        Ok(subvols)
    }

    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> Result<SubvolumeInfo> {
        let (parent, name) = Self::parent_and_name(dest)?;
        let parent_fd = Self::open_dir(parent)?;
        let source_fd = Self::open_dir(source)?;

        ioctl::snap_create(parent_fd.as_fd(), source_fd.as_fd(), name, true, dest)?;

        self.subvolume_info(dest)
    }

    fn create_writable_snapshot(&self, source: &Path, dest: &Path) -> Result<SubvolumeInfo> {
        let (parent, name) = Self::parent_and_name(dest)?;
        let parent_fd = Self::open_dir(parent)?;
        let source_fd = Self::open_dir(source)?;

        ioctl::snap_create(parent_fd.as_fd(), source_fd.as_fd(), name, false, dest)?;

        self.subvolume_info(dest)
    }

    fn create_subvolume(&self, path: &Path) -> Result<()> {
        let (parent, name) = Self::parent_and_name(path)?;
        let parent_fd = Self::open_dir(parent)?;

        ioctl::subvol_create(parent_fd.as_fd(), name, path)
    }

    fn delete_subvolume(&self, path: &Path) -> Result<()> {
        // If readonly, clear the flag first
        let fd = Self::open_dir(path)?;
        let flags = ioctl::get_flags(fd.as_fd(), path)?;
        if flags & ioctl::BTRFS_SUBVOL_RDONLY != 0 {
            ioctl::set_flags(fd.as_fd(), flags & !ioctl::BTRFS_SUBVOL_RDONLY, path)?;
        }
        drop(fd);

        let (parent, name) = Self::parent_and_name(path)?;
        let parent_fd = Self::open_dir(parent)?;

        ioctl::snap_destroy(parent_fd.as_fd(), name, path)
    }

    fn rename_subvolume(&self, source: &Path, dest: &Path) -> Result<()> {
        // On btrfs, renaming a subvolume is just renaming its directory entry.
        fs::rename(source, dest).map_err(|e| RevenantError::io(source, e))
    }

    fn subvolume_info(&self, path: &Path) -> Result<SubvolumeInfo> {
        let fd = Self::open_dir(path)?;
        let info = ioctl::get_subvol_info(fd.as_fd(), path)?;
        let flags = ioctl::get_flags(fd.as_fd(), path)?;

        Ok(SubvolumeInfo {
            id: info.treeid,
            parent_id: info.parent_id,
            path: path.to_path_buf(),
            uuid: Uuid::from_bytes(info.uuid),
            readonly: flags & ioctl::BTRFS_SUBVOL_RDONLY != 0,
        })
    }

    fn set_default_subvolume(&self, path: &Path) -> Result<()> {
        let fd = Self::open_dir(path)?;
        let info = ioctl::get_subvol_info(fd.as_fd(), path)?;
        ioctl::set_default_subvol(fd.as_fd(), info.treeid, path)
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path).map_err(|e| RevenantError::io(path, e))
    }

    fn find_nested_subvolumes(&self, root: &Path) -> Result<Vec<std::path::PathBuf>> {
        let mut nested = Vec::new();
        self.find_nested_recursive(root, &mut nested)?;
        Ok(nested)
    }
}
