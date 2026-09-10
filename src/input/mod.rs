mod actor;
mod keyboard;
mod keymap;
mod layout;
mod rdp;
mod virtual_keyboard;
mod wayland;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardLayoutPolicy {
    Client,
    Compositor,
}

pub(crate) use layout::OutputLayoutSnapshot;
pub(crate) use layout::PreparedOutputLayout;
pub use layout::SharedOutputLayout;
pub(crate) use rdp::RdpInputSessionSink;
pub use wayland::HyprInputHandler;
