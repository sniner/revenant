//! UI building blocks split out of `main.rs`.
//!
//! The submodules group widgets, dialog flows, and small formatting
//! helpers by concern so `main.rs` itself stays focused on app-level
//! plumbing (`AppState`, `Widgets`, the event loop).

pub mod dialogs;
pub mod format;
pub mod snapshots;
pub mod strains;
pub mod toast;
