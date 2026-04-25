//! Shared daemon state.
//!
//! Holds the loaded config and the active toplevel mount guard. The
//! D-Bus interface receives an `Arc<DaemonState>` and reads from it
//! for all method handlers.

use std::sync::Arc;

use revenant_core::Config;

use crate::mount::ToplevelMount;

/// Why the daemon could not establish its full operating state. Each
/// variant maps to a user-visible reason in `GetDaemonInfo`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DegradedReason {
    #[error("config file could not be loaded: {0}")]
    ConfigLoad(String),
    #[error("toplevel mount failed: {0}")]
    Mount(String),
}

pub struct DaemonState {
    pub config: Option<Config>,
    pub toplevel: Option<ToplevelMount>,
    pub degraded: Option<DegradedReason>,
}

impl DaemonState {
    /// Initialise the daemon: load the default config, mount the
    /// toplevel. Either step may fail; in that case the daemon still
    /// runs but reports `degraded` and rejects backend-touching calls.
    pub fn initialize() -> Arc<Self> {
        let (config, mount, degraded) = match Config::load_default() {
            Ok(cfg) => match ToplevelMount::mount(&cfg) {
                Ok(m) => (Some(cfg), Some(m), None),
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::error!("toplevel mount failed: {msg}");
                    (Some(cfg), None, Some(DegradedReason::Mount(msg)))
                }
            },
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::error!("config load failed: {msg}");
                (None, None, Some(DegradedReason::ConfigLoad(msg)))
            }
        };

        Arc::new(Self {
            config,
            toplevel: mount,
            degraded,
        })
    }

    pub fn backend_name(&self) -> &'static str {
        // Single backend today; reflect what `revenant-core` actually
        // supports rather than echoing whatever the config string was.
        "btrfs"
    }
}
