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
// The inbound half of file transfer. Compiled either way rather than gated at
// the twenty-odd sites in the backend that touch it, which would trade one thin
// dispatcher for two. Without the direction nothing mounts, so nothing reads,
// and every seam here is inert.
#[cfg_attr(not(feature = "client-to-server"), allow(dead_code))]
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
