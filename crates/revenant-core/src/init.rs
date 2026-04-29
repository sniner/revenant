//! Auto-detection of system configuration for `revenantctl init`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::{
    BootloaderConfig, Config, EfiConfig, RetainConfig, RootfsConfig, StrainConfig, SysConfig,
};
use crate::error::{Result, RevenantError};

/// Detected system configuration.
#[derive(Debug)]
pub struct DetectedConfig {
    pub backend: String,
    pub device_uuid: String,
    pub rootfs_subvol: String,
    pub efi: Option<DetectedEfi>,
    pub bootloader: Option<String>,
}

/// Detected EFI configuration.
#[derive(Debug)]
pub struct DetectedEfi {
    pub mount_point: String,
}

/// A parsed entry from /proc/self/mountinfo.
#[derive(Debug)]
struct MountInfoEntry {
    root: String,
    mount_point: String,
    fs_type: String,
    mount_source: String,
    #[allow(dead_code)]
    super_options: String,
}

/// Parse /proc/self/mountinfo into structured entries.
fn parse_mountinfo() -> Result<Vec<MountInfoEntry>> {
    let content = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| RevenantError::io(Path::new("/proc/self/mountinfo"), e))?;
    Ok(parse_mountinfo_content(&content))
}

fn parse_mountinfo_content(content: &str) -> Vec<MountInfoEntry> {
    let mut entries = Vec::new();

    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            continue;
        }

        // Fields: id parent_id major:minor root mount_point mount_options [optional_fields...] - fs_type mount_source super_options
        let root = fields[3].to_string();
        let mount_point = fields[4].to_string();

        // Find the '-' separator — it separates optional fields from fs_type, mount_source, super_options
        let sep_pos = fields.iter().position(|&f| f == "-");
        let Some(sep) = sep_pos else { continue };

        if fields.len() < sep + 4 {
            continue;
        }

        let fs_type = fields[sep + 1].to_string();
        let mount_source = fields[sep + 2].to_string();
        let super_options = fields[sep + 3].to_string();

        entries.push(MountInfoEntry {
            root,
            mount_point,
            fs_type,
            mount_source,
            super_options,
        });
    }

    entries
}

/// Detect whether root is on btrfs.
fn detect_backend(entries: &[MountInfoEntry]) -> Option<String> {
    entries
        .iter()
        .find(|e| e.mount_point == "/")
        .filter(|e| e.fs_type == "btrfs")
        .map(|_| "btrfs".to_string())
}

/// Detect the root mount entry: returns (`device_path`, `subvol_name`).
fn detect_root_mount(entries: &[MountInfoEntry]) -> Option<(String, String)> {
    let root_entry = entries.iter().find(|e| e.mount_point == "/")?;

    // The `root` field contains the subvolume path, e.g. "/@" or "/@rootfs"
    let subvol = root_entry.root.trim_start_matches('/').to_string();

    Some((root_entry.mount_source.clone(), subvol))
}

/// Detect the UUID for a given device path by scanning /dev/disk/by-uuid/.
fn detect_device_uuid(device: &str) -> Result<Option<String>> {
    let device_canonical =
        std::fs::canonicalize(device).map_err(|e| RevenantError::io(Path::new(device), e))?;

    let uuid_dir = Path::new("/dev/disk/by-uuid");
    if !uuid_dir.exists() {
        return Ok(None);
    }

    let entries = std::fs::read_dir(uuid_dir).map_err(|e| RevenantError::io(uuid_dir, e))?;

    for entry in entries {
        let entry = entry.map_err(|e| RevenantError::io(uuid_dir, e))?;
        let link_path = entry.path();
        let Ok(target) = std::fs::canonicalize(&link_path) else {
            continue;
        };
        if target != device_canonical {
            continue;
        }
        if let Some(name) = link_path.file_name().and_then(|n| n.to_str()) {
            return Ok(Some(name.to_string()));
        }
    }

    Ok(None)
}

/// Detect an EFI system partition mount (vfat at /boot or /boot/efi).
fn detect_efi(entries: &[MountInfoEntry]) -> Option<DetectedEfi> {
    for mount_point in &["/boot", "/boot/efi", "/efi"] {
        if let Some(entry) = entries
            .iter()
            .find(|e| e.mount_point == *mount_point && e.fs_type == "vfat")
        {
            return Some(DetectedEfi {
                mount_point: entry.mount_point.clone(),
            });
        }
    }
    None
}

/// Detect the bootloader via small filesystem heuristics.
///
/// The detected value is purely informational — revenant's restore
/// mechanism (subvolume rename) is bootloader-agnostic. A `None` return
/// is mapped to `"unknown"` by the caller.
fn detect_bootloader(efi_mount: Option<&str>) -> Option<String> {
    detect_bootloader_with(efi_mount, |p| p.exists())
}

fn detect_bootloader_with<F>(efi_mount: Option<&str>, exists: F) -> Option<String>
where
    F: Fn(&Path) -> bool,
{
    // systemd-boot: loader config on the ESP.
    if let Some(mount) = efi_mount {
        let loader = PathBuf::from(mount).join("loader/loader.conf");
        if exists(&loader) {
            return Some("systemd-boot".to_string());
        }
    }

    // GRUB via its config in /boot — covers BIOS+GRUB as well as most
    // EFI+GRUB distros that keep grub.cfg in /boot even when the ESP is
    // mounted elsewhere (Debian, Ubuntu, Arch, Fedora via grub2, …).
    for path in &["/boot/grub/grub.cfg", "/boot/grub2/grub.cfg"] {
        if exists(Path::new(path)) {
            return Some("grub".to_string());
        }
    }

    // GRUB when the ESP is mounted at /boot and grub.cfg lives on it.
    if let Some(mount) = efi_mount {
        let grub_cfg = PathBuf::from(mount).join("grub/grub.cfg");
        if exists(&grub_cfg) {
            return Some("grub".to_string());
        }
    }

    None
}

/// Orchestrate full system detection.
pub fn detect_all() -> Result<DetectedConfig> {
    let entries = parse_mountinfo()?;

    let backend = detect_backend(&entries)
        .ok_or_else(|| RevenantError::Config("root filesystem is not btrfs".to_string()))?;

    let (device, rootfs_subvol) = detect_root_mount(&entries)
        .ok_or_else(|| RevenantError::Config("could not determine root mount".to_string()))?;

    let device_uuid = detect_device_uuid(&device)?.ok_or_else(|| {
        RevenantError::Config(format!("could not determine UUID for device: {device}"))
    })?;

    let efi = detect_efi(&entries);

    let bootloader = detect_bootloader(efi.as_ref().map(|e| e.mount_point.as_str()));

    Ok(DetectedConfig {
        backend,
        device_uuid,
        rootfs_subvol,
        efi,
        bootloader,
    })
}

/// Build a Config from detected values.
#[must_use]
pub fn build_config(detected: DetectedConfig) -> Config {
    let DetectedConfig {
        backend,
        device_uuid,
        rootfs_subvol,
        efi,
        bootloader,
    } = detected;

    let efi_enabled = efi.is_some();
    let efi_mount = efi.map_or_else(|| PathBuf::from("/boot"), |e| PathBuf::from(e.mount_point));
    let bootloader_backend = bootloader.unwrap_or_else(|| "unknown".to_string());

    let sys = SysConfig {
        rootfs_subvol: rootfs_subvol.clone(),
        snapshot_subvol: "@snapshots".to_string(),
        auto_apply_retention: true,
        tombstone_max_age_days: 14,
        rootfs: RootfsConfig {
            backend,
            device_uuid,
        },
        efi: EfiConfig {
            enabled: efi_enabled,
            mount_point: efi_mount,
            staging_subvol: "@boot".to_string(),
        },
        bootloader: BootloaderConfig {
            backend: bootloader_backend,
        },
    };

    let default_strain = StrainConfig {
        display_name: None,
        retain: RetainConfig::default(),
        subvolumes: vec![rootfs_subvol],
        efi: efi_enabled,
    };

    let mut strain = HashMap::new();
    strain.insert("default".to_string(), default_strain);

    Config { sys, strain }
}

/// Serialize a Config to a TOML string with a header comment.
pub fn config_to_toml(config: &Config) -> Result<String> {
    let toml_str =
        toml::to_string_pretty(config).map_err(|e| RevenantError::Other(e.to_string()))?;
    Ok(format!(
        "# Revenant configuration\n# Generated by: revenantctl init\n# Review and adjust as needed.\n\n{toml_str}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MOUNTINFO: &str = "\
22 1 259:2 /@ / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,ssd,space_cache=v2,subvolid=256,subvol=/@
23 22 259:2 /@home /home rw,relatime shared:2 - btrfs /dev/nvme0n1p2 rw,ssd,space_cache=v2,subvolid=257,subvol=/@home
24 22 259:1 / /boot rw,relatime shared:3 - vfat /dev/nvme0n1p1 rw,fmask=0022,dmask=0022,codepage=437,iocharset=ascii,shortname=mixed,utf8,errors=remount-ro
";

    #[test]
    fn parse_mountinfo_entries() {
        let entries = parse_mountinfo_content(SAMPLE_MOUNTINFO);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].mount_point, "/");
        assert_eq!(entries[0].fs_type, "btrfs");
        assert_eq!(entries[0].root, "/@");
        assert_eq!(entries[1].mount_point, "/home");
        assert_eq!(entries[2].mount_point, "/boot");
        assert_eq!(entries[2].fs_type, "vfat");
    }

    #[test]
    fn detect_backend_btrfs() {
        let entries = parse_mountinfo_content(SAMPLE_MOUNTINFO);
        assert_eq!(detect_backend(&entries), Some("btrfs".to_string()));
    }

    #[test]
    fn detect_root_mount_subvol() {
        let entries = parse_mountinfo_content(SAMPLE_MOUNTINFO);
        let (device, subvol) = detect_root_mount(&entries).unwrap();
        assert_eq!(device, "/dev/nvme0n1p2");
        assert_eq!(subvol, "@");
    }

    #[test]
    fn detect_bootloader_systemd_boot() {
        let exists = |p: &Path| p == Path::new("/boot/loader/loader.conf");
        assert_eq!(
            detect_bootloader_with(Some("/boot"), exists),
            Some("systemd-boot".to_string())
        );
    }

    #[test]
    fn detect_bootloader_grub_bios() {
        let exists = |p: &Path| p == Path::new("/boot/grub/grub.cfg");
        assert_eq!(
            detect_bootloader_with(None, exists),
            Some("grub".to_string())
        );
    }

    #[test]
    fn detect_bootloader_grub2_fedora() {
        let exists = |p: &Path| p == Path::new("/boot/grub2/grub.cfg");
        assert_eq!(
            detect_bootloader_with(Some("/boot/efi"), exists),
            Some("grub".to_string())
        );
    }

    #[test]
    fn detect_bootloader_grub_on_esp() {
        // ESP mounted at /efi, grub.cfg on it.
        let exists = |p: &Path| p == Path::new("/efi/grub/grub.cfg");
        assert_eq!(
            detect_bootloader_with(Some("/efi"), exists),
            Some("grub".to_string())
        );
    }

    #[test]
    fn detect_bootloader_none() {
        let exists = |_: &Path| false;
        assert_eq!(detect_bootloader_with(Some("/boot"), exists), None);
        assert_eq!(detect_bootloader_with(None, |_: &Path| false), None);
    }

    #[test]
    fn detect_efi_mount() {
        let entries = parse_mountinfo_content(SAMPLE_MOUNTINFO);
        let efi = detect_efi(&entries).unwrap();
        assert_eq!(efi.mount_point, "/boot");
    }

    #[test]
    fn build_config_from_detected() {
        let detected = DetectedConfig {
            backend: "btrfs".to_string(),
            device_uuid: "12345678-1234-1234-1234-123456789abc".to_string(),
            rootfs_subvol: "@".to_string(),
            efi: Some(DetectedEfi {
                mount_point: "/boot".to_string(),
            }),
            bootloader: Some("systemd-boot".to_string()),
        };

        let config = build_config(detected);
        assert_eq!(config.sys.rootfs_subvol, "@");
        assert!(config.sys.efi.enabled);
        assert_eq!(config.strain.len(), 1);
        let default = config.strain("default").unwrap();
        assert_eq!(default.retain.last, 10);
        assert!(default.efi);
        assert_eq!(default.subvolumes, vec!["@"]);
    }

    #[test]
    fn config_to_toml_roundtrip() {
        let detected = DetectedConfig {
            backend: "btrfs".to_string(),
            device_uuid: "12345678-1234-1234-1234-123456789abc".to_string(),
            rootfs_subvol: "@".to_string(),
            efi: Some(DetectedEfi {
                mount_point: "/boot".to_string(),
            }),
            bootloader: Some("systemd-boot".to_string()),
        };
        let config = build_config(detected);
        let toml_str = config_to_toml(&config).unwrap();
        // Should be parseable back
        let parsed = &toml_str.parse::<Config>().unwrap();
        assert_eq!(
            parsed.sys.rootfs.device_uuid,
            "12345678-1234-1234-1234-123456789abc"
        );
        assert_eq!(parsed.strain.len(), 1);
    }

    #[test]
    fn example_config_parseable() {
        let example = include_str!("../../../config/revenant.toml.example");
        // The example has a placeholder UUID, so we replace it for parsing
        let fixed = example.replace(
            "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
            "12345678-1234-1234-1234-123456789abc",
        );
        let config = &fixed.parse::<Config>().unwrap();
        assert_eq!(config.sys.rootfs_subvol, "@");
        assert!(config.strain.contains_key("default"));
    }
}
