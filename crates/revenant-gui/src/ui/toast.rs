//! Progress toast helper for in-flight privileged operations.
//!
//! Privileged D-Bus calls go through polkit before the daemon does
//! the actual work. To keep the GUI honest about which phase it is
//! in, every such call shows a toast that starts as
//! "waiting for authentication…" and gets retitled to "<verb>…" once
//! the daemon emits `OperationStarted`. The toast is dismissed by
//! the result handler the moment the call resolves.

use std::cell::RefCell;
use std::rc::Rc;

use crate::AppState;

/// In-flight progress toast plus the label to swap to once the
/// daemon emits `OperationStarted` (i.e. polkit has cleared the
/// call). For toasts that need no swap (e.g. dry-run preflight),
/// pass the same string for both labels — the swap is a visual
/// no-op.
#[derive(Clone)]
pub(crate) struct ProgressToast {
    toast: adw::Toast,
    working_label: String,
}

impl ProgressToast {
    pub(crate) fn dismiss(&self) {
        self.toast.dismiss();
    }
}

/// Build an in-flight progress toast carrying the "auth pending"
/// label initially and the "working" label as the swap target. The
/// daemon's `OperationStarted` signal triggers the swap from the
/// event handler — without it the user would see
/// "waiting for authentication" all the way through the actual
/// subvolume work, which reads as "the GUI hung".
pub(crate) fn show_progress_toast(
    overlay: &adw::ToastOverlay,
    auth_label: &str,
    working_label: &str,
) -> ProgressToast {
    let toast = adw::Toast::builder()
        .title(auth_label)
        .timeout(0) // 0 = do not auto-dismiss; cleared by the result handler
        .build();
    overlay.add_toast(toast.clone());
    ProgressToast {
        toast,
        working_label: working_label.to_string(),
    }
}

/// Swap every active progress toast from its "auth pending" title
/// to its "working" title. Called from the `OperationStarted`
/// event handler — only one toast is `Some` at a time (gates), but
/// walking all four is cheap.
pub(crate) fn apply_operation_started(state: &Rc<RefCell<AppState>>) {
    let st = state.borrow();
    for pt in [
        st.restore_progress_toast.as_ref(),
        st.create_progress_toast.as_ref(),
        st.delete_progress_toast.as_ref(),
        st.purge_progress_toast.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        pt.toast.set_title(&pt.working_label);
    }
}
