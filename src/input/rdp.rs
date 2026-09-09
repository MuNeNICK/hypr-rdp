use std::sync::{mpsc, Weak};

use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};

use super::actor::InputCommand;
use super::keyboard::{generate_xkb_keymap_from_names, xkb_names_for_rdp_keyboard_layout};
use super::wayland::HyprInputHandler;
use super::KeyboardLayoutPolicy;

impl RdpServerInputHandler for HyprInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        self.send_input_command(InputCommand::Keyboard(event));
    }

    fn mouse(&mut self, event: MouseEvent) {
        self.send_input_command(InputCommand::Mouse(event));
    }
}

/// Input-owned adapter for RDP session metadata and lifecycle events.
pub(crate) trait RdpInputSessionSink: Send {
    fn set_keyboard_layout(&self, keyboard_layout: u32);
    fn session_ended(&self);
}

pub(crate) struct RdpInputSessionHandle {
    keyboard_layout_policy: KeyboardLayoutPolicy,
    commands: Weak<mpsc::Sender<InputCommand>>,
}

impl RdpInputSessionHandle {
    pub(super) fn new(
        keyboard_layout_policy: KeyboardLayoutPolicy,
        commands: Weak<mpsc::Sender<InputCommand>>,
    ) -> Self {
        Self {
            keyboard_layout_policy,
            commands,
        }
    }
}

impl RdpInputSessionSink for RdpInputSessionHandle {
    fn session_ended(&self) {
        let Some(commands) = self.commands.upgrade() else {
            return;
        };
        if commands.send(InputCommand::ReleaseHeldKeys).is_err() {
            tracing::warn!("Input actor is gone; keys held at session end stay held");
        }
    }

    fn set_keyboard_layout(&self, keyboard_layout: u32) {
        let Some(keymap_data) =
            client_keymap_from_keyboard_layout(self.keyboard_layout_policy, keyboard_layout)
        else {
            tracing::info!(
                keyboard_layout = %format_args!("{keyboard_layout:#010x}"),
                keyboard_layout_policy = ?self.keyboard_layout_policy,
                "Keeping existing keyboard keymap"
            );
            return;
        };

        tracing::info!(
            keyboard_layout = %format_args!("{keyboard_layout:#010x}"),
            "Applying client keyboard layout"
        );
        let Some(commands) = self.commands.upgrade() else {
            tracing::warn!("Input actor is gone; dropping keyboard layout command");
            return;
        };
        if commands
            .send(InputCommand::ApplyKeymap {
                keymap_data,
                keymap_source: "rdp-client",
            })
            .is_err()
        {
            tracing::warn!("Input actor is gone; dropping keyboard layout command");
        }
    }
}

fn client_keymap_from_keyboard_layout(
    keyboard_layout_policy: KeyboardLayoutPolicy,
    keyboard_layout: u32,
) -> Option<Vec<u8>> {
    if keyboard_layout_policy == KeyboardLayoutPolicy::Compositor {
        return None;
    }

    let names = xkb_names_for_rdp_keyboard_layout(keyboard_layout)?;
    match generate_xkb_keymap_from_names(&names) {
        Ok(keymap) => Some(keymap),
        Err(err) => {
            tracing::warn!(
                keyboard_layout = %format_args!("{keyboard_layout:#010x}"),
                layout = ?names.layout,
                variant = ?names.variant,
                options = ?names.options,
                "Failed to generate XKB keymap from client keyboard layout: {:#}",
                err
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use super::{client_keymap_from_keyboard_layout, RdpInputSessionHandle};
    use crate::input::actor::InputCommand;
    use crate::input::keyboard::KeyboardStateTracker;
    use crate::input::rdp::RdpInputSessionSink;
    use crate::input::wayland::HyprInputHandler;
    use crate::input::KeyboardLayoutPolicy;
    use ironrdp_server::{KeyboardEvent, RdpServerInputHandler};

    #[test]
    fn keyboard_handler_enqueues_exact_event_order() {
        let (commands, receiver) = mpsc::channel();
        let mut handler = HyprInputHandler::test_handler_with_commands(Arc::new(commands));

        handler.keyboard(KeyboardEvent::Pressed {
            code: 0x5b,
            extended: true,
        });
        handler.keyboard(KeyboardEvent::Pressed {
            code: 0x5b,
            extended: true,
        });
        handler.keyboard(KeyboardEvent::Released {
            code: 0x5b,
            extended: true,
        });

        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("first"),
            InputCommand::Keyboard(KeyboardEvent::Pressed {
                code: 0x5b,
                extended: true
            })
        ));
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("second"),
            InputCommand::Keyboard(KeyboardEvent::Pressed {
                code: 0x5b,
                extended: true
            })
        ));
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("third"),
            InputCommand::Keyboard(KeyboardEvent::Released {
                code: 0x5b,
                extended: true
            })
        ));
        assert!(
            receiver.try_recv().is_err(),
            "no extra commands may be enqueued"
        );
    }

    #[test]
    fn client_keyboard_layout_generates_non_us_keymap() {
        let keymap = client_keymap_from_keyboard_layout(KeyboardLayoutPolicy::Client, 0x00000407)
            .expect("German HKL is supported");
        let tracker = KeyboardStateTracker::new(&keymap).expect("generated keymap loads");

        assert_eq!(tracker.unicode_to_evdev('z' as u16).unwrap().evdev_key, 21);
        assert_eq!(tracker.unicode_to_evdev('y' as u16).unwrap().evdev_key, 44);
    }

    #[test]
    fn client_keyboard_layout_keeps_existing_keymap_when_unknown() {
        assert!(
            client_keymap_from_keyboard_layout(KeyboardLayoutPolicy::Client, 0x0000ffff,).is_none()
        );
    }

    #[test]
    fn compositor_keyboard_layout_policy_ignores_supported_client_layout() {
        assert!(
            client_keymap_from_keyboard_layout(KeyboardLayoutPolicy::Compositor, 0x00000407,)
                .is_none()
        );
    }

    #[test]
    fn rdp_input_session_handle_enqueues_session_end() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle =
            RdpInputSessionHandle::new(KeyboardLayoutPolicy::Client, Arc::downgrade(&commands));

        handle.session_ended();

        let command = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the end of a session must reach the input actor");
        assert!(matches!(command, InputCommand::ReleaseHeldKeys));
    }

    #[test]
    fn rdp_input_session_handle_sends_apply_keymap_for_supported_layout() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle =
            RdpInputSessionHandle::new(KeyboardLayoutPolicy::Client, Arc::downgrade(&commands));

        handle.set_keyboard_layout(0x00000407);

        let command = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("command");
        assert!(
            matches!(
                command,
                InputCommand::ApplyKeymap {
                    keymap_source: "rdp-client",
                    ..
                }
            ),
            "supported client HKL must enqueue an ApplyKeymap command"
        );
    }

    #[test]
    fn rdp_input_session_handle_keeps_existing_keymap_when_unknown() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle =
            RdpInputSessionHandle::new(KeyboardLayoutPolicy::Client, Arc::downgrade(&commands));

        handle.set_keyboard_layout(0x0000ffff);

        assert!(
            receiver.try_recv().is_err(),
            "unknown client HKL must not enqueue a keymap command"
        );
    }

    #[test]
    fn compositor_policy_ignores_supported_client_layout() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle =
            RdpInputSessionHandle::new(KeyboardLayoutPolicy::Compositor, Arc::downgrade(&commands));

        handle.set_keyboard_layout(0x00000407);

        assert!(
            receiver.try_recv().is_err(),
            "compositor policy must not enqueue a keymap command"
        );
    }

    #[test]
    fn rdp_input_session_handle_does_not_keep_input_actor_alive() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle =
            RdpInputSessionHandle::new(KeyboardLayoutPolicy::Client, Arc::downgrade(&commands));

        drop(commands);

        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        handle.set_keyboard_layout(0x00000407);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }
}
