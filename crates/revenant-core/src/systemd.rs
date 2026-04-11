use std::path::{Path, PathBuf};

/// Parameters for generating systemd unit files.
pub struct SystemdUnitParams {
    pub bin_path: PathBuf,
    pub config_path: PathBuf,
    pub boot_strain: String,
    pub periodic_strain: String,
    pub timer_calendar: String,
}

/// A generated systemd unit file.
pub struct UnitFile {
    pub filename: String,
    pub content: String,
}

fn exec_start(bin_path: &Path, config_path: &Path, strain: &str) -> String {
    format!(
        "{} --config {} snapshot --strain {}",
        bin_path.display(),
        config_path.display(),
        strain,
    )
}

/// Generate systemd unit files for boot and periodic snapshots.
#[must_use]
pub fn generate_units(params: &SystemdUnitParams) -> Vec<UnitFile> {
    let boot_service = UnitFile {
        filename: "revenant-boot.service".to_string(),
        content: format!(
            "\
[Unit]
Description=Revenant boot snapshot
After=local-fs.target

[Service]
Type=oneshot
ExecStart={}

[Install]
WantedBy=multi-user.target
",
            exec_start(&params.bin_path, &params.config_path, &params.boot_strain),
        ),
    };

    let periodic_service = UnitFile {
        filename: "revenant-periodic.service".to_string(),
        content: format!(
            "\
[Unit]
Description=Revenant periodic snapshot

[Service]
Type=oneshot
ExecStart={}
",
            exec_start(
                &params.bin_path,
                &params.config_path,
                &params.periodic_strain,
            ),
        ),
    };

    let periodic_timer = UnitFile {
        filename: "revenant-periodic.timer".to_string(),
        content: format!(
            "\
[Unit]
Description=Revenant periodic snapshot timer

[Timer]
OnCalendar={calendar}
Persistent=true

[Install]
WantedBy=timers.target
",
            calendar = params.timer_calendar,
        ),
    };

    vec![boot_service, periodic_service, periodic_timer]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_params() -> SystemdUnitParams {
        SystemdUnitParams {
            bin_path: PathBuf::from("/usr/local/bin/revenantctl"),
            config_path: PathBuf::from("/etc/revenant/config.toml"),
            boot_strain: "default".to_string(),
            periodic_strain: "periodic".to_string(),
            timer_calendar: "hourly".to_string(),
        }
    }

    #[test]
    fn generates_three_units() {
        let units = generate_units(&test_params());
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].filename, "revenant-boot.service");
        assert_eq!(units[1].filename, "revenant-periodic.service");
        assert_eq!(units[2].filename, "revenant-periodic.timer");
    }

    #[test]
    fn boot_service_content() {
        let units = generate_units(&test_params());
        let content = &units[0].content;
        assert!(content.contains("Type=oneshot"));
        assert!(content.contains("WantedBy=multi-user.target"));
        assert!(content.contains("snapshot --strain default"));
        assert!(content.contains("/usr/local/bin/revenantctl --config /etc/revenant/config.toml"));
    }

    #[test]
    fn periodic_service_content() {
        let units = generate_units(&test_params());
        let content = &units[1].content;
        assert!(content.contains("Type=oneshot"));
        assert!(content.contains("snapshot --strain periodic"));
        assert!(!content.contains("[Install]"));
    }

    #[test]
    fn periodic_timer_content() {
        let units = generate_units(&test_params());
        let content = &units[2].content;
        assert!(content.contains("OnCalendar=hourly"));
        assert!(content.contains("Persistent=true"));
        assert!(content.contains("WantedBy=timers.target"));
    }

    #[test]
    fn custom_timer_interval() {
        let mut params = test_params();
        params.timer_calendar = "*-*-* 00/4:00:00".to_string();
        let units = generate_units(&params);
        assert!(units[2].content.contains("OnCalendar=*-*-* 00/4:00:00"));
    }

    #[test]
    fn custom_strains() {
        let mut params = test_params();
        params.boot_strain = "boot".to_string();
        params.periodic_strain = "hourly".to_string();
        let units = generate_units(&params);
        assert!(units[0].content.contains("snapshot --strain boot"));
        assert!(units[1].content.contains("snapshot --strain hourly"));
    }
}
