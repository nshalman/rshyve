// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The input side of the RDP server.
//!
//! Keys arrive as Scan Code Set 1 codes and go to the keyboard as they
//! are. The mouse is absolute and drives the tablet, whose PointerSink
//! takes VNC's button mask: bit 0 left, 1 middle, 2 right, 3 wheel up,
//! 4 wheel down.

use std::sync::Arc;

use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use vmm_devices::display::DisplaySource;
use vmm_devices::input::scale_to_tablet;
use vmm_devices::InputBroker;

const LEFT: u8 = 1 << 0;
const MIDDLE: u8 = 1 << 1;
const RIGHT: u8 = 1 << 2;
const WHEEL_UP: u8 = 1 << 3;
const WHEEL_DOWN: u8 = 1 << 4;

/// RDP's scroll amount for one wheel notch.
const NOTCH: i32 = 120;

pub(crate) struct Input {
    input: Arc<InputBroker>,
    /// The display the pointer moves over, for scaling.
    source: Arc<dyn DisplaySource>,
    /// Buttons held, as a VNC mask.
    buttons: u8,
    /// The last position, in display pixels.
    x: u16,
    y: u16,
    /// Scroll not yet sent as a step, in RDP's units (120 a notch).
    scroll: i32,
    log: slog::Logger,
    /// Whether this connection was told about Unicode keys yet.
    unicode_logged: bool,
}

impl Input {
    pub(crate) fn new(
        input: Arc<InputBroker>,
        source: Arc<dyn DisplaySource>,
        log: slog::Logger,
    ) -> Self {
        Self {
            input,
            source,
            buttons: 0,
            x: 0,
            y: 0,
            scroll: 0,
            log,
            unicode_logged: false,
        }
    }

    /// Send the pointer with `extra` buttons held on top of the real ones.
    fn pointer(&self, extra: u8) {
        let (width, height) = self.source.size();
        let extent = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
        self.input.pointer_event(
            self.buttons | extra,
            scale_to_tablet(self.x, extent(width)),
            scale_to_tablet(self.y, extent(height)),
        );
    }

    fn button(&mut self, bit: u8, down: bool) {
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        self.pointer(0);
    }
}

impl RdpServerInputHandler for Input {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                self.input.scancode_event(true, code, extended);
            }
            KeyboardEvent::Released { code, extended } => {
                self.input.scancode_event(false, code, extended);
            }
            // A client sends Unicode for keys it has no scan code for,
            // or for every key in its Unicode keyboard mode, and a PS/2
            // keyboard cannot type them. Said once a connection, so a
            // keyboard that does nothing has a cause in the log.
            KeyboardEvent::UnicodePressed(_)
            | KeyboardEvent::UnicodeReleased(_) => {
                if !self.unicode_logged {
                    self.unicode_logged = true;
                    slog::warn!(self.log, "RDP client sent Unicode keys, which a PS/2 keyboard cannot type; use its scan code keyboard mode");
                }
            }
            // Lock state follows the guest's own LEDs.
            KeyboardEvent::Synchronize(_) => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => {
                (self.x, self.y) = (x, y);
                self.pointer(0);
            }
            MouseEvent::LeftPressed => self.button(LEFT, true),
            MouseEvent::LeftReleased => self.button(LEFT, false),
            MouseEvent::MiddlePressed => self.button(MIDDLE, true),
            MouseEvent::MiddleReleased => self.button(MIDDLE, false),
            MouseEvent::RightPressed => self.button(RIGHT, true),
            MouseEvent::RightReleased => self.button(RIGHT, false),
            MouseEvent::VerticalScroll { value } => {
                // A notch is 120. A trackpad sends less at a time, so
                // amounts add up and each whole notch is one step.
                self.scroll = self.scroll.saturating_add(i32::from(value));
                while self.scroll.abs() >= NOTCH {
                    let (wheel, notch) = if self.scroll > 0 {
                        (WHEEL_UP, NOTCH)
                    } else {
                        (WHEEL_DOWN, -NOTCH)
                    };
                    self.scroll -= notch;
                    self.pointer(wheel);
                    self.pointer(0);
                }
            }
            // The tablet has no side buttons, no horizontal wheel and no
            // relative motion.
            MouseEvent::Button4Pressed
            | MouseEvent::Button4Released
            | MouseEvent::Button5Pressed
            | MouseEvent::Button5Released
            | MouseEvent::Scroll { .. }
            | MouseEvent::RelMove { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{FakeDisplay, Recorder};

    fn input() -> (Arc<Recorder>, Input) {
        logged_input(slog::Logger::root(slog::Discard, slog::o!()))
    }

    fn logged_input(log: slog::Logger) -> (Arc<Recorder>, Input) {
        let rec = Arc::new(Recorder::default());
        let broker = Arc::new(InputBroker::default());
        broker.set_keyboard(rec.clone());
        broker.set_pointer(rec.clone());
        let input =
            Input::new(broker, Arc::new(FakeDisplay::new(1025, 769)), log);
        (rec, input)
    }

    /// Counts the warnings logged to it.
    struct Warnings(Arc<std::sync::atomic::AtomicUsize>);

    impl slog::Drain for Warnings {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            _values: &slog::OwnedKVList,
        ) -> Result<(), slog::Never> {
            if record.level() == slog::Level::Warning {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(())
        }
    }

    /// A client in Unicode keyboard mode types nothing a PS/2 keyboard
    /// can send: the log says so, once a connection, so a dead keyboard
    /// has a cause.
    #[test]
    fn unicode_keys_are_logged_once() {
        let warnings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let log =
            slog::Logger::root(Warnings(Arc::clone(&warnings)), slog::o!());
        let (rec, mut input) = logged_input(log);
        for _ in 0..3 {
            input.keyboard(KeyboardEvent::UnicodePressed(u16::from(b'a')));
            input.keyboard(KeyboardEvent::UnicodeReleased(u16::from(b'a')));
        }
        assert_eq!(warnings.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(rec.scancodes.lock().unwrap().is_empty());
    }

    #[test]
    fn keys_go_as_scan_codes() {
        let (rec, mut input) = input();
        input.keyboard(KeyboardEvent::Pressed {
            code: 0x1e,
            extended: false,
        });
        input.keyboard(KeyboardEvent::Released {
            code: 0x4d,
            extended: true,
        });
        assert_eq!(
            *rec.scancodes.lock().unwrap(),
            [(true, 0x1e, false), (false, 0x4d, true)]
        );
    }

    /// Moves are absolute, scaled to the tablet's range against the
    /// display's size; buttons use the VNC mask the tablet takes.
    #[test]
    fn the_mouse_drives_the_tablet() {
        let (rec, mut input) = input();
        input.mouse(MouseEvent::Move { x: 1024, y: 384 });
        input.mouse(MouseEvent::LeftPressed);
        input.mouse(MouseEvent::RightPressed);
        input.mouse(MouseEvent::LeftReleased);
        input.mouse(MouseEvent::MiddlePressed);
        assert_eq!(
            *rec.pointer.lock().unwrap(),
            [
                (0b000, 0x7FFF, 0x3FFF),
                (0b001, 0x7FFF, 0x3FFF),
                (0b101, 0x7FFF, 0x3FFF),
                (0b100, 0x7FFF, 0x3FFF),
                (0b110, 0x7FFF, 0x3FFF),
            ]
        );
    }

    /// A wheel step is a press and release of the wheel button, as VNC
    /// sends it: bit 3 up, bit 4 down.
    #[test]
    fn the_wheel_steps_the_wheel_buttons() {
        let (rec, mut input) = input();
        input.mouse(MouseEvent::VerticalScroll { value: 120 });
        input.mouse(MouseEvent::VerticalScroll { value: -120 });
        let buttons: Vec<u8> =
            rec.pointer.lock().unwrap().iter().map(|e| e.0).collect();
        assert_eq!(buttons, [0b01000, 0, 0b10000, 0]);
    }

    /// A notch is 120. A trackpad sends many smaller amounts, which add
    /// up to steps rather than each being a whole notch.
    #[test]
    fn small_scroll_amounts_add_up_to_steps() {
        let (rec, mut input) = input();
        for _ in 0..6 {
            input.mouse(MouseEvent::VerticalScroll { value: 40 });
        }
        input.mouse(MouseEvent::VerticalScroll { value: -30 });
        let buttons: Vec<u8> =
            rec.pointer.lock().unwrap().iter().map(|e| e.0).collect();
        assert_eq!(buttons, [0b01000, 0, 0b01000, 0], "240 is two steps");
    }

    #[test]
    fn a_fast_flick_is_several_steps() {
        let (rec, mut input) = input();
        input.mouse(MouseEvent::VerticalScroll { value: -360 });
        let buttons: Vec<u8> =
            rec.pointer.lock().unwrap().iter().map(|e| e.0).collect();
        assert_eq!(buttons, [0b10000, 0, 0b10000, 0, 0b10000, 0]);
    }
}
