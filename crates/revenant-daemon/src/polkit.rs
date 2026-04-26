//! PolicyKit1 client.
//!
//! Thin wrapper around `org.freedesktop.PolicyKit1.Authority` so the
//! D-Bus handlers can ask "is this caller allowed to perform action
//! X?" and get a yes/no — possibly after an interactive auth prompt
//! shown by the user's session agent.
//!
//! The polkit action ids match `data/org.revenant.policy`.

use std::collections::HashMap;

use zbus::proxy;
use zvariant::Value;

use crate::errors::{DaemonError, DaemonResult};

/// Tell polkit it may show an auth dialog if the caller is allowed to
/// authenticate. Without this flag, polkit returns "not authorized"
/// instead of prompting.
const ALLOW_USER_INTERACTION: u32 = 1;

/// `org.freedesktop.PolicyKit1.Authority` proxy.
///
/// We only use `CheckAuthorization`. Other methods (registering
/// authentication agents, enumerating actions) are out of scope.
#[proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
pub trait Authority {
    /// Wire signature: `(sa{sv})sa{ss}us → (bba{ss})`.
    fn check_authorization(
        &self,
        subject: (&str, HashMap<&str, Value<'_>>),
        action_id: &str,
        details: HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<(bool, bool, HashMap<String, String>)>;
}

/// Check whether the caller identified by `bus_name` (a unique
/// system-bus name like `:1.42`) may perform `action_id`.
///
/// Returns `Ok(())` on grant, `DaemonError::NotAuthorized` on denial.
/// `is_challenge` (i.e. polkit started but did not complete an
/// auth dialog) is treated as denial — the caller can simply retry.
pub async fn check(conn: &zbus::Connection, bus_name: &str, action_id: &str) -> DaemonResult<()> {
    let proxy = AuthorityProxy::new(conn)
        .await
        .map_err(|e| DaemonError::Internal(format!("polkit proxy: {e}")))?;

    let subject_details = HashMap::from([("name", Value::from(bus_name))]);
    let subject = ("system-bus-name", subject_details);

    let (is_authorized, _is_challenge, _details) = proxy
        .check_authorization(
            subject,
            action_id,
            HashMap::new(),
            ALLOW_USER_INTERACTION,
            "",
        )
        .await
        .map_err(|e| DaemonError::Internal(format!("polkit check {action_id}: {e}")))?;

    if is_authorized {
        Ok(())
    } else {
        Err(DaemonError::NotAuthorized(format!(
            "not authorized for action {action_id}"
        )))
    }
}
