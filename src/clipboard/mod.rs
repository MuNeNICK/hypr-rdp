//! Clipboard sharing via wlr-data-control-v1 Wayland protocol and ironrdp-cliprdr.
//!
//! Uses the `zwlr_data_control_manager_v1` protocol natively (no external CLI tools).
//! A dedicated thread runs a Wayland event loop to monitor clipboard changes and
//! handle data transfer via pipe fds.
//!
//! Supports text (CF_UNICODETEXT) and images (CF_DIB via PNG conversion).

mod backend;
mod files;
mod formats;
mod inbound;
mod wayland;

pub use backend::HyprCliprdrFactory;
