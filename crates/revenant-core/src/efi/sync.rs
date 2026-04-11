//! Pure-Rust file synchronization for the EFI system partition.
//!
//! This replaces rsync with a simple recursive sync that:
//! 1. Copies new files from source to destination
//! 2. Overwrites changed files (by mtime/size comparison)
//! 3. Removes files in destination that no longer exist in source
//! 4. Preserves permissions and timestamps
//!
//! Changed files are written block-by-block: only blocks that differ from the
//! destination are written. On Btrfs this avoids creating new extents for
//! unchanged data, so consecutive snapshots of the staging subvolume share the
//! maximum amount of storage.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{Result, RevenantError};

/// Sync files from `source` to `dest`, making `dest` a mirror of `source`.
pub fn sync_to_staging(source: &Path, dest: &Path) -> Result<()> {
    tracing::debug!("syncing {} → {}", source.display(), dest.display());

    if !source.exists() {
        return Err(RevenantError::EfiSync(format!(
            "source does not exist: {}",
            source.display()
        )));
    }

    if !dest.exists() {
        fs::create_dir_all(dest).map_err(|e| RevenantError::io(dest, e))?;
    }

    sync_dir_recursive(source, dest)?;
    remove_stale_recursive(source, dest)?;

    tracing::debug!("sync complete");
    Ok(())
}

/// Recursively copy new/changed files from source to dest.
fn sync_dir_recursive(source: &Path, dest: &Path) -> Result<()> {
    let entries = fs::read_dir(source).map_err(|e| RevenantError::io(source, e))?;

    for entry in entries {
        let entry = entry.map_err(|e| RevenantError::io(source, e))?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dest.join(&file_name);

        let file_type = entry
            .file_type()
            .map_err(|e| RevenantError::io(&src_path, e))?;

        if file_type.is_dir() {
            if !dest_path.exists() {
                fs::create_dir_all(&dest_path).map_err(|e| RevenantError::io(&dest_path, e))?;
            }
            sync_dir_recursive(&src_path, &dest_path)?;
        } else if file_type.is_file() && needs_copy(&src_path, &dest_path)? {
            tracing::trace!("copying {} → {}", src_path.display(), dest_path.display());
            copy_blocks(&src_path, &dest_path)?;
            copy_metadata(&src_path, &dest_path)?;
        }
        // Skip symlinks and special files on ESP
    }

    Ok(())
}

/// Check whether a file needs to be copied (different size or mtime).
fn needs_copy(src: &Path, dest: &Path) -> Result<bool> {
    let src_meta = fs::metadata(src).map_err(|e| RevenantError::io(src, e))?;

    match fs::metadata(dest) {
        Ok(dest_meta) => {
            if src_meta.len() != dest_meta.len() {
                return Ok(true);
            }
            let src_modified = src_meta.modified().map_err(|e| RevenantError::io(src, e))?;
            let dest_modified = dest_meta
                .modified()
                .map_err(|e| RevenantError::io(dest, e))?;
            Ok(src_modified != dest_modified)
        }
        Err(_) => Ok(true), // dest doesn't exist
    }
}

/// Read as many bytes as possible into `buf`, returning the number read.
/// Returns 0 only at EOF. Handles short reads that `Read::read` may produce.
fn read_block(file: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match file.read(&mut buf[total..])? {
            0 => break,
            n => total += n,
        }
    }
    Ok(total)
}

/// Copy `src` to `dest` writing only blocks that differ from existing content.
///
/// On Btrfs, a `write()` always creates a new extent, even if the data is
/// identical to what was already there. By skipping identical blocks we leave
/// the corresponding extents in `dest` untouched, so the next snapshot shares
/// them with the current one instead of duplicating them.
fn copy_blocks(src: &Path, dest: &Path) -> Result<()> {
    const BLOCK_SIZE: usize = 64 * 1024; // 64 KiB — matches typical Btrfs extent granularity

    let src_len = fs::metadata(src)
        .map_err(|e| RevenantError::io(src, e))?
        .len();

    let mut src_file = fs::File::open(src).map_err(|e| RevenantError::io(src, e))?;
    let mut dest_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dest)
        .map_err(|e| RevenantError::io(dest, e))?;

    let dest_len = dest_file
        .metadata()
        .map_err(|e| RevenantError::io(dest, e))?
        .len();

    let mut src_buf = vec![0u8; BLOCK_SIZE];
    let mut dest_buf = vec![0u8; BLOCK_SIZE];
    let mut pos = 0u64;

    loop {
        let n = read_block(&mut src_file, &mut src_buf).map_err(|e| RevenantError::io(src, e))?;
        if n == 0 {
            break;
        }

        // Compare with the existing block in dest (if any).
        // dest_file position tracks pos: after a match the read advanced it by n;
        // after a write we seeked to pos and wrote n bytes — both leave it at pos+n.
        let identical = if pos < dest_len {
            let m = read_block(&mut dest_file, &mut dest_buf[..n])
                .map_err(|e| RevenantError::io(dest, e))?;
            m == n && dest_buf[..n] == src_buf[..n]
        } else {
            false
        };

        if !identical {
            dest_file
                .seek(SeekFrom::Start(pos))
                .map_err(|e| RevenantError::io(dest, e))?;
            dest_file
                .write_all(&src_buf[..n])
                .map_err(|e| RevenantError::io(dest, e))?;
        }

        pos += n as u64;
    }

    // Trim dest if the source file shrank since the last sync.
    if dest_len > src_len {
        dest_file
            .set_len(src_len)
            .map_err(|e| RevenantError::io(dest, e))?;
    }

    Ok(())
}

/// Copy file permissions and timestamps.
fn copy_metadata(src: &Path, dest: &Path) -> Result<()> {
    let meta = fs::metadata(src).map_err(|e| RevenantError::io(src, e))?;
    fs::set_permissions(dest, meta.permissions()).map_err(|e| RevenantError::io(dest, e))?;

    // Copy modification time using utimensat
    if let Ok(mtime) = meta.modified() {
        if let Ok(duration) = mtime.duration_since(std::time::UNIX_EPOCH) {
            let ts = nix::sys::time::TimeSpec::new(
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
                i64::from(duration.subsec_nanos()),
            );
            if let Err(e) = nix::sys::stat::utimensat(
                None,
                dest,
                &ts,
                &ts,
                nix::sys::stat::UtimensatFlags::FollowSymlink,
            ) {
                tracing::warn!("failed to set mtime on {}: {e}", dest.display());
            }
        }
    }

    Ok(())
}

/// Remove files/dirs in dest that no longer exist in source.
fn remove_stale_recursive(source: &Path, dest: &Path) -> Result<()> {
    let source_entries: HashSet<_> = fs::read_dir(source)
        .map_err(|e| RevenantError::io(source, e))?
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .collect();

    let dest_entries = fs::read_dir(dest).map_err(|e| RevenantError::io(dest, e))?;

    for entry in dest_entries {
        let entry = entry.map_err(|e| RevenantError::io(dest, e))?;
        let name = entry.file_name();

        if source_entries.contains(&name) {
            // Recurse into matching directories
            let src_path = source.join(&name);
            let dest_path = dest.join(&name);
            if dest_path.is_dir() && src_path.is_dir() {
                remove_stale_recursive(&src_path, &dest_path)?;
            }
        } else {
            let path = entry.path();
            tracing::trace!("removing stale {}", path.display());
            if path.is_dir() {
                fs::remove_dir_all(&path).map_err(|e| RevenantError::io(&path, e))?;
            } else {
                fs::remove_file(&path).map_err(|e| RevenantError::io(&path, e))?;
            }
        }
    }

    Ok(())
}
