//! Typed `zbus` proxy for the `dev.sniner.Revenant1` interface.
//!
//! Mirrors the daemon-side `#[interface]` declarations in
//! `crates/revenant-daemon/src/dbus.rs`. Wire types are kept as
//! `HashMap<String, OwnedValue>` (`a{sv}`) and tuples; richer
//! domain types (e.g. parsed `Snapshot`/`Strain` structs) live a
//! layer above and decode out of these.
//!
//! Custom errors arrive as `zbus::Error::MethodError(name, …)` —
//! the wire name (`dev.sniner.Revenant.Error.NotFound`, etc.) is the
//! authoritative discriminator. Translating them into a typed
//! client-side error enum is deliberately deferred; the GUI's
//! error-handling slice will model that once it has concrete UX
//! requirements.

use std::collections::HashMap;

use zbus::proxy;
use zvariant::OwnedValue;

/// Extensible `a{sv}` dict — used for snapshot/daemon-info/options.
pub type Dict = HashMap<String, OwnedValue>;

/// Strain wire tuple — `(sasba{sv}s)`: name, subvolumes, efi,
/// retention, display_name. `display_name` is `""` when not set in
/// config; the GUI treats `""` and absent as identical.
#[allow(dead_code)]
pub type StrainTuple = (String, Vec<String>, bool, Dict, String);

#[proxy(
    interface = "dev.sniner.Revenant1",
    default_service = "dev.sniner.Revenant",
    default_path = "/dev/sniner/Revenant"
)]
pub trait Daemon {
    // -- Discovery / metadata ------------------------------------------

    fn get_version(&self) -> zbus::Result<String>;

    fn get_daemon_info(&self) -> zbus::Result<Dict>;

    // -- Strains -------------------------------------------------------

    fn list_strains(&self) -> zbus::Result<Vec<StrainTuple>>;

    fn get_strain(&self, name: &str) -> zbus::Result<StrainTuple>;

    fn get_latest_strain(&self) -> zbus::Result<String>;

    fn set_strain_retention(&self, name: &str, retention: Dict) -> zbus::Result<()>;

    // -- Snapshots -----------------------------------------------------

    fn list_snapshots(&self, filter: Dict) -> zbus::Result<Vec<Dict>>;

    fn get_snapshot(&self, strain: &str, id: &str) -> zbus::Result<Dict>;

    fn get_live_parent(&self) -> zbus::Result<Dict>;

    fn create_snapshot(&self, strain: &str, message: Vec<String>) -> zbus::Result<Dict>;

    fn delete_snapshot(&self, strain: &str, id: &str) -> zbus::Result<()>;

    fn restore(&self, strain: &str, id: &str, options: Dict) -> zbus::Result<Dict>;

    // -- DELETE markers -----------------------------------------------

    fn list_delete_markers(&self) -> zbus::Result<Vec<Dict>>;

    fn purge_delete_markers(&self, names: Vec<String>) -> zbus::Result<Vec<String>>;

    // -- Signals -------------------------------------------------------

    #[zbus(signal)]
    fn snapshots_changed(&self, strain: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn strain_config_changed(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn live_parent_changed(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn daemon_state_changed(&self, state: String, message: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn delete_markers_changed(&self) -> zbus::Result<()>;
}
