//! Round-trip editing of `/etc/revenant/config.toml`.
//!
//! Privileged write paths edit the config file in place, preserving
//! comments, key order, formatting whitespace, and untouched
//! sections. We use `toml_edit::DocumentMut` for that — `toml`'s
//! deserialise-then-reserialise loses formatting.
//!
//! Writes are atomic: the new content goes to `<path>.tmp`, the file
//! is `fsync`'d, and then renamed over the original.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use revenant_core::RetainConfig;

/// Default location for the live revenant config.
pub const CONFIG_PATH: &str = "/etc/revenant/config.toml";

/// Replace `[strain.<name>.retain]` in the config file with the given
/// retention. Keys with value 0 are *removed* from the section, since
/// 0 ≡ "disabled" in the on-disk format and a zero-valued key would
/// be surprising in a manually-edited file.
///
/// Errors out cleanly if the named strain is missing — adding strains
/// is intentionally not a daemon-side operation.
pub fn set_strain_retention(strain: &str, retain: &RetainConfig) -> Result<()> {
    set_strain_retention_at(Path::new(CONFIG_PATH), strain, retain)
}

fn set_strain_retention_at(path: &Path, strain: &str, retain: &RetainConfig) -> Result<()> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .with_context(|| format!("parse {} as TOML", path.display()))?;

    let strain_section = doc
        .get_mut("strain")
        .and_then(|i| i.as_table_mut())
        .ok_or_else(|| anyhow!("config has no [strain.*] section"))?;

    let strain_table = strain_section
        .get_mut(strain)
        .and_then(|i| i.as_table_mut())
        .ok_or_else(|| anyhow!("unknown strain: {strain}"))?;

    // Implicit table entry => `strain_table["retain"] = toml_edit::table();`
    // would force `[strain.<name>.retain]` to be re-emitted as an
    // inline-table-style assignment. The Item::or_insert_with helper
    // creates a header-style table only if the key is absent.
    let retain_table = match strain_table.entry("retain") {
        toml_edit::Entry::Occupied(e) => e
            .into_mut()
            .as_table_mut()
            .ok_or_else(|| anyhow!("[strain.{strain}.retain] is not a table"))?,
        toml_edit::Entry::Vacant(e) => e
            .insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .expect("just inserted a Table"),
    };

    set_or_remove(retain_table, "last", retain.last);
    set_or_remove(retain_table, "hourly", retain.hourly);
    set_or_remove(retain_table, "daily", retain.daily);
    set_or_remove(retain_table, "weekly", retain.weekly);
    set_or_remove(retain_table, "monthly", retain.monthly);
    set_or_remove(retain_table, "yearly", retain.yearly);

    write_atomic(path, doc.to_string().as_bytes())?;
    Ok(())
}

fn set_or_remove(table: &mut toml_edit::Table, key: &str, value: usize) {
    if value == 0 {
        table.remove(key);
    } else {
        table[key] = toml_edit::value(i64::try_from(value).unwrap_or(i64::MAX));
    }
}

/// Write `bytes` to `path` atomically: tempfile → fsync → rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(content: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn rewrites_existing_retain_table() {
        let f = write_temp(
            r#"
[strain.default]
subvolumes = ["@"]

[strain.default.retain]
last = 3
daily = 7
"#,
        );
        let retain = RetainConfig {
            last: 5,
            hourly: 24,
            daily: 0,
            weekly: 0,
            monthly: 0,
            yearly: 0,
        };
        set_strain_retention_at(f.path(), "default", &retain).unwrap();
        let out = std::fs::read_to_string(f.path()).unwrap();
        assert!(out.contains("last = 5"));
        assert!(out.contains("hourly = 24"));
        // daily was 7, set to 0 → key removed.
        assert!(!out.contains("daily"));
    }

    #[test]
    fn creates_retain_table_if_missing() {
        let f = write_temp(
            r#"
[strain.boot]
subvolumes = ["@"]
efi = false
"#,
        );
        let retain = RetainConfig {
            last: 10,
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: 0,
            yearly: 0,
        };
        set_strain_retention_at(f.path(), "boot", &retain).unwrap();
        let out = std::fs::read_to_string(f.path()).unwrap();
        assert!(out.contains("[strain.boot.retain]") || out.contains("retain"));
        assert!(out.contains("last = 10"));
    }

    #[test]
    fn unknown_strain_errors() {
        let f = write_temp(
            r#"
[strain.default]
subvolumes = ["@"]
"#,
        );
        let retain = RetainConfig::default();
        let err = set_strain_retention_at(f.path(), "ghost", &retain).unwrap_err();
        assert!(err.to_string().contains("unknown strain"));
    }

    #[test]
    fn preserves_comments_in_unrelated_sections() {
        let f = write_temp(
            r#"
# important note about defaults
[strain.default]
subvolumes = ["@"]

# tier explanation
[strain.default.retain]
last = 3
"#,
        );
        let retain = RetainConfig {
            last: 7,
            ..RetainConfig::default()
        };
        set_strain_retention_at(f.path(), "default", &retain).unwrap();
        let out = std::fs::read_to_string(f.path()).unwrap();
        assert!(out.contains("# important note"));
        assert!(out.contains("# tier explanation"));
        assert!(out.contains("last = 7"));
    }
}
