//! User-facing dialog flows: build the dialog, dispatch the command,
//! render the result toast. Each submodule owns one operation.

pub mod cleanup;
pub mod create;
pub mod delete;
pub mod restore;
pub mod retention;
