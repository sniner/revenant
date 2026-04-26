//! Custom `org.revenant.Error.*` D-Bus errors.
//!
//! Per `docs/design/dbus-interface.md`, application-level failures go
//! through one of these variants instead of `org.freedesktop.DBus
//! .Error.Failed`. The `zbus::DBusError` derive maps each variant to a
//! wire name in our namespace, so clients can dispatch on the error
//! name rather than parsing the human message.
//!
//! Protocol-level zbus errors (transport failures, type mismatches the
//! framework catches before our handlers run) flow through the
//! `ZBus(zbus::Error)` pass-through variant unchanged.

use zbus::DBusError;

#[derive(Debug, DBusError)]
#[zbus(prefix = "org.revenant.Error")]
pub enum DaemonError {
    /// Pass-through for standard zbus errors (transport, marshalling,
    /// peer disconnects). Required by the derive macro for non-namespace
    /// errors to round-trip cleanly.
    #[zbus(error)]
    ZBus(zbus::Error),

    /// Polkit denied the caller (or no auth agent could prompt).
    NotAuthorized(String),

    /// Strain or snapshot id not found in the requested scope.
    /// Snapshot lookups are strain-scoped: the same id under a
    /// different strain still surfaces as `NotFound`.
    NotFound(String),

    /// Application-level argument validation failed: malformed
    /// snapshot id, retention tier with a non-u32 value, etc.
    /// Distinct from protocol-level type errors that zbus catches
    /// before the handler runs.
    InvalidArgument(String),

    /// Restore preflight check (e.g. `/run/systemd/machine-id`
    /// integrity) reported `Severity::Error` findings. The caller
    /// should `dry_run` again to read the findings list and surface
    /// it to the user before retrying.
    PreflightBlocked(String),

    /// Reserved for future serialization conflicts on write paths
    /// (e.g. a second `Restore` while one is in flight). Not yet
    /// emitted — write-path serialization is still a TODO.
    Conflict(String),

    /// Toplevel mount not held, config not loaded, or backend
    /// otherwise unable to serve a request that needs the filesystem.
    BackendUnavailable(String),

    /// Catch-all for daemon-internal failures (encoding bugs, polkit
    /// proxy errors, btrfs ioctl quirks). Clients should surface the
    /// message and treat the call as failed but retryable.
    Internal(String),
}

pub type DaemonResult<T> = std::result::Result<T, DaemonError>;

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::DBusError;

    #[test]
    fn variants_render_under_revenant_namespace() {
        // Every variant rename is a wire-format change; this guards
        // against accidentally breaking the contract in the design doc.
        let cases = [
            (
                DaemonError::NotAuthorized("x".into()),
                "org.revenant.Error.NotAuthorized",
            ),
            (
                DaemonError::NotFound("x".into()),
                "org.revenant.Error.NotFound",
            ),
            (
                DaemonError::InvalidArgument("x".into()),
                "org.revenant.Error.InvalidArgument",
            ),
            (
                DaemonError::PreflightBlocked("x".into()),
                "org.revenant.Error.PreflightBlocked",
            ),
            (
                DaemonError::Conflict("x".into()),
                "org.revenant.Error.Conflict",
            ),
            (
                DaemonError::BackendUnavailable("x".into()),
                "org.revenant.Error.BackendUnavailable",
            ),
            (
                DaemonError::Internal("x".into()),
                "org.revenant.Error.Internal",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.name().as_str(), expected, "for {err:?}");
        }
    }
}
