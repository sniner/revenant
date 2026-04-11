use std::path::{Path, PathBuf};

use super::{BootEntry, BootloaderBackend};
use crate::error::{Result, RevenantError};
use crate::snapshot::SnapshotId;

/// systemd-boot backend.
pub struct SystemdBootBackend {
    esp_path: PathBuf,
}

impl SystemdBootBackend {
    #[must_use]
    pub fn new(esp_path: PathBuf) -> Self {
        Self { esp_path }
    }

    fn entries_dir(&self) -> PathBuf {
        self.esp_path.join("loader").join("entries")
    }

    fn rollback_entry_path(&self, id: &SnapshotId) -> PathBuf {
        self.entries_dir()
            .join(format!("revenant-rollback-{id}.conf"))
    }
}

impl BootloaderBackend for SystemdBootBackend {
    fn detect(&self) -> Result<bool> {
        // systemd-boot is present if the loader directory exists
        Ok(self.esp_path.join("loader").join("loader.conf").exists())
    }

    fn efi_partition_path(&self) -> Result<PathBuf> {
        Ok(self.esp_path.clone())
    }

    fn list_entries(&self) -> Result<Vec<BootEntry>> {
        let dir = self.entries_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        let dir_entries = std::fs::read_dir(&dir).map_err(|e| RevenantError::io(&dir, e))?;

        for entry in dir_entries {
            let entry = entry.map_err(|e| RevenantError::io(&dir, e))?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "conf") {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let title = parse_entry_title(&path).unwrap_or_else(|| id.clone());
                entries.push(BootEntry { id, title, path });
            }
        }

        entries.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(entries)
    }

    fn create_rollback_entry(&self, snapshot_id: &SnapshotId, rootfs_subvol: &str) -> Result<()> {
        let path = self.rollback_entry_path(snapshot_id);

        let content = format!(
            "title   Revenant Rollback ({snapshot_id})\n\
             linux   /vmlinuz-linux\n\
             initrd  /initramfs-linux.img\n\
             options rootflags=subvol={rootfs_subvol} rw\n"
        );

        let dir = self.entries_dir();
        std::fs::create_dir_all(&dir).map_err(|e| RevenantError::io(&dir, e))?;
        std::fs::write(&path, content).map_err(|e| RevenantError::io(&path, e))?;

        tracing::info!("created rollback entry: {}", path.display());
        Ok(())
    }

    fn remove_rollback_entry(&self, snapshot_id: &SnapshotId) -> Result<()> {
        let path = self.rollback_entry_path(snapshot_id);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| RevenantError::io(&path, e))?;
            tracing::info!("removed rollback entry: {}", path.display());
        }
        Ok(())
    }
}

/// Parse the title from a systemd-boot entry file.
fn parse_entry_title(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("title") {
            return Some(rest.trim().to_string());
        }
    }
    None
}
