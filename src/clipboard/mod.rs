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
#[cfg(feature = "client-to-server")]
mod mount;
mod remote;
#[cfg(feature = "client-to-server")]
mod remote_tree;
mod wayland;

pub use backend::HyprCliprdrFactory;

/// Removes remote-file mounts left behind by a server that exited without
/// unmounting. Called once at start; a no-op where the direction that mounts
/// is not compiled in.
pub fn sweep_orphan_mounts() {
    #[cfg(feature = "client-to-server")]
    mount::sweep_orphan_mounts();
}
