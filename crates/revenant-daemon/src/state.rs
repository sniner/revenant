//! Shared daemon state.
//!
//! Holds the loaded config and the active toplevel mount guard. The
//! D-Bus interface receives an `Arc<DaemonState>` and reads from it
//! for all method handlers.
//!
//! `config` lives behind a `tokio::sync::RwLock` so privileged write
//! paths (e.g. `SetStrainRetention`) can replace it after editing
//! `/etc/revenant/config.toml`. `toplevel`, `backend` and `degraded`
//! are set once at startup and never mutate.

use std::path::Path;
use std::sync::Arc;

use revenant_core::Config;
use revenant_core::backend::btrfs::BtrfsBackend;
use tokio::sync::{RwLock, RwLockReadGuard};

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
    pub config: RwLock<Option<Config>>,
    pub toplevel: Option<ToplevelMount>,
    pub backend: BtrfsBackend,
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
            config: RwLock::new(config),
            toplevel: mount,
            backend: BtrfsBackend::new(),
            degraded,
        })
    }

    /// Acquire a read-lock on config and verify the backend is fully
    /// up. Returns a guard that holds the read-lock and exposes both
    /// the config and the toplevel path.
    pub async fn ready(&self) -> Result<ReadyState<'_>, zbus::fdo::Error> {
        let config_guard = self.config.read().await;
        let toplevel = self
            .toplevel
            .as_ref()
            .ok_or_else(|| self.backend_unavailable())?;
        if config_guard.is_none() {
            return Err(self.backend_unavailable());
        }
        Ok(ReadyState {
            config: config_guard,
            toplevel: toplevel.path(),
        })
    }

    /// Replace the cached config with a freshly-loaded copy.  Used by
    /// privileged write paths after they have rewritten
    /// `/etc/revenant/config.toml`.
    pub async fn reload_config(&self) -> anyhow::Result<()> {
        let new = Config::load_default()?;
        *self.config.write().await = Some(new);
        Ok(())
    }

    pub fn backend_name(&self) -> &'static str {
        // Single backend today; reflect what `revenant-core` actually
        // supports rather than echoing whatever the config string was.
        "btrfs"
    }

    fn backend_unavailable(&self) -> zbus::fdo::Error {
        zbus::fdo::Error::Failed(format!(
            "backend unavailable: {}",
            self.degraded
                .as_ref()
                .map_or_else(|| "unknown".to_string(), ToString::to_string)
        ))
    }
}

/// Read-side handle returned by [`DaemonState::ready`]. Holds the
/// config read-lock for as long as it is alive — keep it short so
/// concurrent callers don't queue behind a single slow handler.
pub struct ReadyState<'a> {
    config: RwLockReadGuard<'a, Option<Config>>,
    toplevel: &'a Path,
}

impl<'a> ReadyState<'a> {
    pub fn config(&self) -> &Config {
        self.config
            .as_ref()
            .expect("ReadyState invariant: config is Some")
    }

    pub fn toplevel(&self) -> &'a Path {
        self.toplevel
    }
}
