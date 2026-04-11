use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, RevenantError};

/// Default configuration file path.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/revenant/config.toml";

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub sys: SysConfig,
    #[serde(default)]
    pub strain: HashMap<String, StrainConfig>,
}

/// System-level configuration grouped under `[sys]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SysConfig {
    /// Subvolume holding the OS rootfs (e.g. "@").
    #[serde(default = "default_rootfs_subvol")]
    pub rootfs_subvol: String,
    /// Subvolume where snapshots are stored (e.g. "@snapshots").
    #[serde(default = "default_snapshot_subvol")]
    pub snapshot_subvol: String,
    pub rootfs: RootfsConfig,
    pub efi: EfiConfig,
    pub bootloader: BootloaderConfig,
}

fn default_rootfs_subvol() -> String {
    "@".to_string()
}

fn default_snapshot_subvol() -> String {
    "@snapshots".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootfsConfig {
    /// Backend type (currently only "btrfs").
    #[serde(default = "default_backend")]
    pub backend: String,
    /// UUID of the btrfs device.
    pub device_uuid: String,
}

fn default_backend() -> String {
    "btrfs".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EfiConfig {
    /// Whether EFI backup is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Mount point of the EFI/boot partition.
    #[serde(default = "default_efi_mount")]
    pub mount_point: PathBuf,
    /// Staging subvolume for EFI content.
    #[serde(default = "default_staging_subvol")]
    pub staging_subvol: String,
}

fn default_efi_mount() -> PathBuf {
    PathBuf::from("/boot")
}

fn default_staging_subvol() -> String {
    "@boot".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootloaderConfig {
    /// Bootloader backend (currently only "systemd-boot").
    #[serde(default = "default_bootloader_backend")]
    pub backend: String,
}

fn default_bootloader_backend() -> String {
    "systemd-boot".to_string()
}

/// Tiered retention policy for a snapshot strain.
///
/// All fields default to 0 (disabled). When the entire `[strain.x.retain]` section is
/// absent, `RetainConfig::default()` applies (`last = 10`, all others 0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainConfig {
    /// Keep N most-recent snapshots regardless of age.
    #[serde(default)]
    pub last: usize,
    /// Keep the newest snapshot per clock-hour for N hours.
    #[serde(default)]
    pub hourly: usize,
    /// Keep the newest snapshot per calendar-day for N days.
    #[serde(default)]
    pub daily: usize,
    /// Keep the newest snapshot per ISO-week for N weeks.
    #[serde(default)]
    pub weekly: usize,
    /// Keep the newest snapshot per calendar-month for N months.
    #[serde(default)]
    pub monthly: usize,
    /// Keep the newest snapshot per calendar-year for N years.
    #[serde(default)]
    pub yearly: usize,
}

impl Default for RetainConfig {
    fn default() -> Self {
        Self {
            last: 10,
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: 0,
            yearly: 0,
        }
    }
}

/// Configuration for a snapshot strain (namespace).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrainConfig {
    /// Retention policy for this strain.
    #[serde(default)]
    pub retain: RetainConfig,
    /// Subvolumes to snapshot.
    pub subvolumes: Vec<String>,
    /// Whether to sync EFI as part of this strain's snapshots.
    #[serde(default)]
    pub efi: bool,
}

/// Reserved pseudo-strain name used to mark subvolumes pending deletion.
/// Must not be used as a real strain name in configuration.
pub const DELETE_STRAIN: &str = "DELETE";

/// Valid strain names: only alphanumeric and underscore. Hyphens are excluded
/// because they appear in the snapshot naming scheme as delimiters, and the
/// reserved name DELETE must not be usable.
fn is_valid_strain_name(name: &str) -> bool {
    !name.is_empty()
        && name != DELETE_STRAIN
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl Config {
    /// Load configuration from the given path.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| RevenantError::io(path, e))?;
        content.parse()
    }

    /// Load configuration from the default path.
    pub fn load_default() -> Result<Self> {
        Self::load(Path::new(DEFAULT_CONFIG_PATH))
    }

    /// Parse configuration from a TOML string.
    pub fn parse(s: &str) -> Result<Self> {
        s.parse()
    }

    /// Look up a strain by name.
    pub fn strain(&self, name: &str) -> Result<&StrainConfig> {
        self.strain
            .get(name)
            .ok_or_else(|| RevenantError::Config(format!("unknown strain: {name}")))
    }

    /// Validate the configuration.
    fn validate(&self) -> Result<()> {
        if self.sys.rootfs_subvol.is_empty() {
            return Err(RevenantError::Config(
                "sys.rootfs_subvol must not be empty".to_string(),
            ));
        }
        if self.sys.snapshot_subvol.is_empty() {
            return Err(RevenantError::Config(
                "sys.snapshot_subvol must not be empty".to_string(),
            ));
        }
        if self.sys.rootfs.backend != "btrfs" {
            return Err(RevenantError::Config(format!(
                "unsupported rootfs backend: {}",
                self.sys.rootfs.backend
            )));
        }
        if self.sys.bootloader.backend != "systemd-boot" {
            return Err(RevenantError::Config(format!(
                "unsupported bootloader backend: {}",
                self.sys.bootloader.backend
            )));
        }
        if self.strain.is_empty() {
            return Err(RevenantError::Config(
                "at least one strain must be defined".to_string(),
            ));
        }
        for (name, strain) in &self.strain {
            if !is_valid_strain_name(name) {
                return Err(RevenantError::Config(format!(
                    "invalid strain name '{name}': only [a-zA-Z0-9_] allowed"
                )));
            }
            let r = &strain.retain;
            if r.last == 0
                && r.hourly == 0
                && r.daily == 0
                && r.weekly == 0
                && r.monthly == 0
                && r.yearly == 0
            {
                return Err(RevenantError::Config(format!(
                    "strain '{name}': retain config must have at least one field > 0"
                )));
            }
            if strain.subvolumes.is_empty() {
                return Err(RevenantError::Config(format!(
                    "strain '{name}': subvolumes must not be empty"
                )));
            }
            if strain.efi && !self.sys.efi.enabled {
                return Err(RevenantError::Config(format!(
                    "strain '{name}' has efi = true but sys.efi.enabled is false"
                )));
            }
        }
        Ok(())
    }
}

impl std::str::FromStr for Config {
    type Err = RevenantError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let config: Config = toml::from_str(s).map_err(|e| RevenantError::Config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_CONFIG: &str = r#"
[sys]
rootfs_subvol = "@"

[sys.rootfs]
backend = "btrfs"
device_uuid = "12345678-1234-1234-1234-123456789abc"

[sys.efi]
enabled = true
mount_point = "/boot"
staging_subvol = "@boot"

[sys.bootloader]
backend = "systemd-boot"

[strain.default]
subvolumes = ["@"]
efi = true

[strain.default.retain]
last = 5
"#;

    #[test]
    fn parse_example_config() {
        let config = EXAMPLE_CONFIG.parse::<Config>().unwrap();
        assert_eq!(config.sys.rootfs_subvol, "@");
        assert!(config.sys.efi.enabled);
        let default_strain = config.strain("default").unwrap();
        assert_eq!(default_strain.retain.last, 5);
        assert_eq!(default_strain.subvolumes, vec!["@"]);
        assert!(default_strain.efi);
    }

    #[test]
    fn no_retain_section_defaults_to_last_10() {
        let toml = r#"
[sys]
rootfs_subvol = "@"

[sys.rootfs]
backend = "btrfs"
device_uuid = "12345678-1234-1234-1234-123456789abc"

[sys.efi]
enabled = false

[sys.bootloader]
backend = "systemd-boot"

[strain.default]
subvolumes = ["@"]
"#;
        let config = toml.parse::<Config>().unwrap();
        let s = config.strain("default").unwrap();
        assert_eq!(s.retain.last, 10);
        assert_eq!(s.retain.daily, 0);
    }

    #[test]
    fn partial_retain_config() {
        let toml = r#"
[sys]
rootfs_subvol = "@"

[sys.rootfs]
backend = "btrfs"
device_uuid = "12345678-1234-1234-1234-123456789abc"

[sys.efi]
enabled = false

[sys.bootloader]
backend = "systemd-boot"

[strain.default]
subvolumes = ["@"]

[strain.default.retain]
daily = 7
"#;
        let config = toml.parse::<Config>().unwrap();
        let s = config.strain("default").unwrap();
        assert_eq!(s.retain.last, 0);
        assert_eq!(s.retain.daily, 7);
    }

    #[test]
    fn reject_all_zero_retain() {
        let toml = r#"
[sys]
rootfs_subvol = "@"

[sys.rootfs]
backend = "btrfs"
device_uuid = "12345678-1234-1234-1234-123456789abc"

[sys.efi]
enabled = false

[sys.bootloader]
backend = "systemd-boot"

[strain.default]
subvolumes = ["@"]

[strain.default.retain]
last = 0
"#;
        assert!(toml.parse::<Config>().is_err());
    }

    #[test]
    fn reject_unknown_backend() {
        let toml = EXAMPLE_CONFIG.replace("backend = \"btrfs\"", "backend = \"zfs\"");
        assert!(&toml.parse::<Config>().is_err());
    }

    #[test]
    fn reject_no_strains() {
        let toml = r#"
[sys]
rootfs_subvol = "@"

[sys.rootfs]
device_uuid = "12345678-1234-1234-1234-123456789abc"

[sys.efi]
enabled = false

[sys.bootloader]
backend = "systemd-boot"
"#;
        assert!(toml.parse::<Config>().is_err());
    }

    #[test]
    fn reject_empty_subvolumes() {
        let toml = EXAMPLE_CONFIG.replace("subvolumes = [\"@\"]", "subvolumes = []");
        assert!(&toml.parse::<Config>().is_err());
    }

    #[test]
    fn reject_efi_strain_without_sys_efi() {
        let toml = EXAMPLE_CONFIG.replace("enabled = true", "enabled = false");
        assert!(&toml.parse::<Config>().is_err());
    }

    #[test]
    fn reject_invalid_strain_name() {
        let toml = EXAMPLE_CONFIG.replace("[strain.default]", "[strain.\"bad name!\"]");
        assert!(&toml.parse::<Config>().is_err());
    }

    #[test]
    fn reject_hyphen_in_strain_name() {
        let toml = EXAMPLE_CONFIG.replace("[strain.default]", "[strain.my-strain]");
        assert!(&toml.parse::<Config>().is_err());
    }

    #[test]
    fn reject_reserved_delete_strain_name() {
        let toml = EXAMPLE_CONFIG.replace("[strain.default]", "[strain.DELETE]");
        assert!(&toml.parse::<Config>().is_err());
    }

    #[test]
    fn multi_strain_config() {
        let toml = r#"
[sys]
rootfs_subvol = "@"

[sys.rootfs]
device_uuid = "12345678-1234-1234-1234-123456789abc"

[sys.efi]
enabled = true
mount_point = "/boot"
staging_subvol = "@boot"

[sys.bootloader]
backend = "systemd-boot"

[strain.default]
subvolumes = ["@"]
efi = true

[strain.default.retain]
last = 5

[strain.pacman]
subvolumes = ["@"]
efi = false

[strain.pacman.retain]
last = 3
"#;
        let config = toml.parse::<Config>().unwrap();
        assert_eq!(config.strain.len(), 2);
        assert_eq!(config.strain("pacman").unwrap().retain.last, 3);
        assert!(!config.strain("pacman").unwrap().efi);
    }

    #[test]
    fn strain_lookup_not_found() {
        let config = EXAMPLE_CONFIG.parse::<Config>().unwrap();
        assert!(config.strain("nonexistent").is_err());
    }
}
