//! In-memory `FileSystemBackend` implementation for unit tests.
//!
//! Tracks subvolumes in a `HashMap` keyed by absolute path. Mirrors the
//! observable behaviour of [`crate::backend::btrfs::BtrfsBackend`] closely
//! enough that orchestration code in `snapshot.rs`, `cleanup.rs`,
//! `restore.rs` and `check.rs` can be exercised without touching real
//! btrfs ioctls.
//!
//! Intentional simplifications:
//! - `parent_id` is fixed to 5 (the btrfs toplevel id) for everything,
//!   since none of the orchestration logic inspects it.
//! - `delete_subvolume` simulates btrfs ENOTEMPTY behaviour by failing
//!   when any other tracked subvolume has a path under the target.
//! - `list_subvolumes(root)` returns the *direct* path-children of `root`,
//!   matching `BtrfsBackend::list_subvolumes`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use uuid::Uuid;

use crate::backend::{FileSystemBackend, SubvolumeInfo};
use crate::error::{Result, RevenantError};

/// In-memory subvolume record.
#[derive(Debug, Clone)]
struct MockSubvol {
    id: u64,
    uuid: Uuid,
    readonly: bool,
}

#[derive(Debug, Default)]
struct MockState {
    next_id: u64,
    subvols: HashMap<PathBuf, MockSubvol>,
    default_subvol: Option<PathBuf>,
    /// Records every path passed to `create_dir_all`, in call order.
    /// The mock does not actually track regular directories — this
    /// recording exists so unit tests can verify that the orchestration
    /// code actually asks the backend to materialise the parent path
    /// before re-attaching a nested subvolume.
    created_dirs: Vec<PathBuf>,
}

impl MockState {
    fn new() -> Self {
        // btrfs assigns 256 to the first user subvolume; mirror that for
        // realism, even though no test currently checks the value.
        Self {
            next_id: 256,
            ..Self::default()
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn to_info(&self, path: &Path, sv: &MockSubvol) -> SubvolumeInfo {
        SubvolumeInfo {
            id: sv.id,
            parent_id: 5,
            path: path.to_path_buf(),
            uuid: sv.uuid,
            readonly: sv.readonly,
        }
    }
}

/// Test-only backend that records all operations in memory.
#[derive(Debug)]
pub struct MockBackend {
    state: Mutex<MockState>,
}

impl MockBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MockState::new()),
        }
    }

    /// Pre-populate a subvolume at the given path. Convenience for tests
    /// that want a known starting state without going through the trait.
    pub fn seed_subvolume(&self, path: impl Into<PathBuf>) {
        let path = path.into();
        let mut state = self.state.lock().unwrap();
        let id = state.alloc_id();
        state.subvols.insert(
            path,
            MockSubvol {
                id,
                uuid: Uuid::new_v4(),
                readonly: false,
            },
        );
    }

    /// Return all tracked subvolume paths, sorted, for assertions.
    pub fn all_paths(&self) -> Vec<PathBuf> {
        let state = self.state.lock().unwrap();
        let mut v: Vec<_> = state.subvols.keys().cloned().collect();
        v.sort();
        v
    }

    /// Return whether the given path is currently tracked as a subvolume.
    pub fn contains(&self, path: impl AsRef<Path>) -> bool {
        self.state
            .lock()
            .unwrap()
            .subvols
            .contains_key(path.as_ref())
    }

    /// Return the path that was most recently passed to
    /// `set_default_subvolume`, if any.
    pub fn default_subvolume(&self) -> Option<PathBuf> {
        self.state.lock().unwrap().default_subvol.clone()
    }

    /// Return the list of paths that have been passed to
    /// `create_dir_all`, in call order.  The mock does not simulate
    /// regular directories, so this exists purely as a test hook.
    pub fn created_dirs(&self) -> Vec<PathBuf> {
        self.state.lock().unwrap().created_dirs.clone()
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystemBackend for MockBackend {
    fn probe(&self, _path: &Path) -> Result<bool> {
        Ok(true)
    }

    fn list_subvolumes(&self, root: &Path) -> Result<Vec<SubvolumeInfo>> {
        let state = self.state.lock().unwrap();
        let mut out = Vec::new();
        for (path, sv) in &state.subvols {
            // Direct children of root only.
            if path.parent() == Some(root) {
                out.push(state.to_info(path, sv));
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> Result<SubvolumeInfo> {
        let mut state = self.state.lock().unwrap();
        if !state.subvols.contains_key(source) {
            return Err(RevenantError::SubvolumeNotFound(source.to_path_buf()));
        }
        if state.subvols.contains_key(dest) {
            return Err(RevenantError::Other(format!(
                "destination already exists: {}",
                dest.display()
            )));
        }
        let id = state.alloc_id();
        let sv = MockSubvol {
            id,
            uuid: Uuid::new_v4(),
            readonly: true,
        };
        state.subvols.insert(dest.to_path_buf(), sv.clone());
        Ok(state.to_info(dest, &sv))
    }

    fn create_writable_snapshot(&self, source: &Path, dest: &Path) -> Result<SubvolumeInfo> {
        let mut state = self.state.lock().unwrap();
        if !state.subvols.contains_key(source) {
            return Err(RevenantError::SubvolumeNotFound(source.to_path_buf()));
        }
        if state.subvols.contains_key(dest) {
            return Err(RevenantError::Other(format!(
                "destination already exists: {}",
                dest.display()
            )));
        }
        let id = state.alloc_id();
        let sv = MockSubvol {
            id,
            uuid: Uuid::new_v4(),
            readonly: false,
        };
        state.subvols.insert(dest.to_path_buf(), sv.clone());
        Ok(state.to_info(dest, &sv))
    }

    fn create_subvolume(&self, path: &Path) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.subvols.contains_key(path) {
            return Err(RevenantError::Other(format!(
                "already exists: {}",
                path.display()
            )));
        }
        let id = state.alloc_id();
        state.subvols.insert(
            path.to_path_buf(),
            MockSubvol {
                id,
                uuid: Uuid::new_v4(),
                readonly: false,
            },
        );
        Ok(())
    }

    fn delete_subvolume(&self, path: &Path) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if !state.subvols.contains_key(path) {
            return Err(RevenantError::SubvolumeNotFound(path.to_path_buf()));
        }
        // Simulate btrfs ENOTEMPTY: refuse if any other subvol lives under
        // this one (path-prefix match on a path component boundary).
        let has_child = state
            .subvols
            .keys()
            .any(|p| p != path && p.starts_with(path));
        if has_child {
            return Err(RevenantError::Other(format!(
                "ENOTEMPTY: {} contains nested subvolumes",
                path.display()
            )));
        }
        state.subvols.remove(path);
        Ok(())
    }

    fn rename_subvolume(&self, source: &Path, dest: &Path) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if !state.subvols.contains_key(source) {
            return Err(RevenantError::SubvolumeNotFound(source.to_path_buf()));
        }
        if state.subvols.contains_key(dest) {
            return Err(RevenantError::Other(format!(
                "destination already exists: {}",
                dest.display()
            )));
        }
        // On real btrfs, renaming a subvolume is just a directory entry
        // update — nested subvolumes inside `source` come along for the
        // ride because their directory entries live in the renamed tree.
        // We simulate that by re-keying any subvolume whose path starts
        // with `source` onto the equivalent path under `dest`.
        let to_move: Vec<PathBuf> = state
            .subvols
            .keys()
            .filter(|p| p.starts_with(source))
            .cloned()
            .collect();
        for old in to_move {
            let rel = old.strip_prefix(source).unwrap();
            let new = if rel.as_os_str().is_empty() {
                dest.to_path_buf()
            } else {
                dest.join(rel)
            };
            let sv = state.subvols.remove(&old).unwrap();
            state.subvols.insert(new, sv);
        }
        Ok(())
    }

    fn subvolume_info(&self, path: &Path) -> Result<SubvolumeInfo> {
        let state = self.state.lock().unwrap();
        state
            .subvols
            .get(path)
            .map(|sv| state.to_info(path, sv))
            .ok_or_else(|| RevenantError::SubvolumeNotFound(path.to_path_buf()))
    }

    fn set_default_subvolume(&self, path: &Path) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if !state.subvols.contains_key(path) {
            return Err(RevenantError::SubvolumeNotFound(path.to_path_buf()));
        }
        state.default_subvol = Some(path.to_path_buf());
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        // The mock does not track plain directories, so we just
        // record the call for test assertions.
        self.state
            .lock()
            .unwrap()
            .created_dirs
            .push(path.to_path_buf());
        Ok(())
    }

    fn find_nested_subvolumes(&self, root: &Path) -> Result<Vec<std::path::PathBuf>> {
        let state = self.state.lock().unwrap();
        // Direct nested children only — i.e. subvolumes that live under
        // `root` and have no other subvolume between themselves and `root`.
        // Mirrors the BtrfsBackend semantics, which stops walking at every
        // subvolume boundary it encounters.
        let mut nested = Vec::new();
        for p in state.subvols.keys() {
            if p == root || !p.starts_with(root) {
                continue;
            }
            let has_intermediate = state.subvols.keys().any(|other| {
                other != p && other != root && other.starts_with(root) && p.starts_with(other)
            });
            if !has_intermediate {
                nested.push(p.clone());
            }
        }
        Ok(nested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_lookup() {
        let mock = MockBackend::new();
        mock.create_subvolume(Path::new("/top/@")).unwrap();
        let info = mock.subvolume_info(Path::new("/top/@")).unwrap();
        assert!(!info.readonly);
        assert_eq!(info.parent_id, 5);
    }

    #[test]
    fn list_returns_direct_children_only() {
        let mock = MockBackend::new();
        mock.create_subvolume(Path::new("/top/@")).unwrap();
        mock.create_subvolume(Path::new("/top/@home")).unwrap();
        mock.create_subvolume(Path::new("/top/@snapshots")).unwrap();
        mock.create_subvolume(Path::new("/top/@snapshots/@-default-20260316-143022"))
            .unwrap();

        let top_children = mock.list_subvolumes(Path::new("/top")).unwrap();
        assert_eq!(top_children.len(), 3);

        let snap_children = mock.list_subvolumes(Path::new("/top/@snapshots")).unwrap();
        assert_eq!(snap_children.len(), 1);
    }

    #[test]
    fn ro_snapshot_marks_readonly() {
        let mock = MockBackend::new();
        mock.create_subvolume(Path::new("/top/@")).unwrap();
        mock.create_readonly_snapshot(Path::new("/top/@"), Path::new("/top/@-snap"))
            .unwrap();
        let info = mock.subvolume_info(Path::new("/top/@-snap")).unwrap();
        assert!(info.readonly);
    }

    #[test]
    fn snapshot_of_missing_source_fails() {
        let mock = MockBackend::new();
        let err = mock
            .create_readonly_snapshot(Path::new("/top/@"), Path::new("/top/@-snap"))
            .unwrap_err();
        assert!(matches!(err, RevenantError::SubvolumeNotFound(_)));
    }

    #[test]
    fn delete_with_nested_fails() {
        let mock = MockBackend::new();
        mock.create_subvolume(Path::new("/top/@")).unwrap();
        mock.create_subvolume(Path::new("/top/@/var/lib/portables"))
            .unwrap();
        let err = mock.delete_subvolume(Path::new("/top/@")).unwrap_err();
        assert!(format!("{err}").contains("ENOTEMPTY"));
    }

    #[test]
    fn delete_leaf_succeeds() {
        let mock = MockBackend::new();
        mock.create_subvolume(Path::new("/top/@")).unwrap();
        mock.delete_subvolume(Path::new("/top/@")).unwrap();
        assert!(!mock.contains("/top/@"));
    }

    #[test]
    fn set_default_tracked() {
        let mock = MockBackend::new();
        mock.create_subvolume(Path::new("/top/@")).unwrap();
        mock.set_default_subvolume(Path::new("/top/@")).unwrap();
        assert_eq!(mock.default_subvolume(), Some(PathBuf::from("/top/@")));
    }
}
