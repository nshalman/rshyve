// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Late-bound routing for remote console input devices.

use std::sync::{Arc, Mutex};

pub trait KeyboardSink: Send + Sync + 'static {
    /// A key by X11 keysym, as VNC sends it.
    fn key_event(&self, down: bool, keysym: u32);

    /// A key by Scan Code Set 1 make code, with the E0 prefix given as
    /// `extended`, as RDP sends it.
    fn scancode_event(&self, down: bool, code: u8, extended: bool);
}

pub trait PointerSink: Send + Sync + 'static {
    /// Absolute coordinates, already scaled to 0..=0x7FFF.
    fn pointer_event(&self, buttons: u8, x: u16, y: u16);
}

/// Scale a pointer coordinate within `extent` pixels to the tablet's
/// 0..=0x7FFF range, which [`PointerSink`] takes.
pub fn scale_to_tablet(v: u16, extent: u16) -> u16 {
    let divisor = u32::from(extent.saturating_sub(1)).max(1);
    ((u32::from(v) * 0x7FFF) / divisor).min(0x7FFF) as u16
}

/// Routes remote console input (VNC, RDP) to devices created from
/// independent `-s` flags.
#[derive(Default)]
pub struct InputBroker {
    kbd: Mutex<Option<Arc<dyn KeyboardSink>>>,
    ptr: Mutex<Option<Arc<dyn PointerSink>>>,
}

impl InputBroker {
    pub fn set_keyboard(&self, k: Arc<dyn KeyboardSink>) {
        *self.kbd.lock().expect("input broker keyboard lock") = Some(k);
    }

    pub fn set_pointer(&self, p: Arc<dyn PointerSink>) {
        *self.ptr.lock().expect("input broker pointer lock") = Some(p);
    }

    pub fn key_event(&self, down: bool, keysym: u32) {
        let sink = self.kbd.lock().expect("input broker keyboard lock").clone();
        if let Some(sink) = sink {
            sink.key_event(down, keysym);
        }
    }

    pub fn scancode_event(&self, down: bool, code: u8, extended: bool) {
        let sink = self.kbd.lock().expect("input broker keyboard lock").clone();
        if let Some(sink) = sink {
            sink.scancode_event(down, code, extended);
        }
    }

    pub fn pointer_event(&self, buttons: u8, x: u16, y: u16) {
        let sink = self.ptr.lock().expect("input broker pointer lock").clone();
        if let Some(sink) = sink {
            sink.pointer_event(buttons, x, y);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingKeyboard(
        Mutex<Vec<(bool, u32)>>,
        Mutex<Vec<(bool, u8, bool)>>,
    );

    impl RecordingKeyboard {
        fn events(&self) -> Vec<(bool, u32)> {
            self.0.lock().expect("recording keyboard lock").clone()
        }
    }

    impl KeyboardSink for RecordingKeyboard {
        fn key_event(&self, down: bool, keysym: u32) {
            self.0
                .lock()
                .expect("recording keyboard lock")
                .push((down, keysym));
        }

        fn scancode_event(&self, down: bool, code: u8, extended: bool) {
            self.1
                .lock()
                .expect("recording keyboard lock")
                .push((down, code, extended));
        }
    }

    #[test]
    fn scancode_events_reach_sink() {
        let broker = InputBroker::default();
        let kbd = Arc::new(RecordingKeyboard::default());
        broker.set_keyboard(kbd.clone());
        broker.scancode_event(true, 0x4d, true);
        assert_eq!(
            *kbd.1.lock().expect("recording keyboard lock"),
            [(true, 0x4d, true)]
        );
    }

    #[test]
    fn absolute_pointer_scaling() {
        assert_eq!(scale_to_tablet(0, 1024), 0);
        assert_eq!(scale_to_tablet(1023, 1024), 0x7FFF);

        let midpoint = scale_to_tablet(512, 1024);
        assert!((0x3FE0..=0x4020).contains(&midpoint));
    }

    #[test]
    fn absolute_pointer_scaling_handles_degenerate_extents() {
        assert_eq!(scale_to_tablet(0, 0), 0);
        assert_eq!(scale_to_tablet(5, 1), 0x7FFF);
    }

    #[test]
    fn absolute_pointer_scaling_never_exceeds_tablet_range() {
        for extent in [0, 1, 2, 768, 1024, u16::MAX] {
            for value in 0..=u16::MAX {
                assert!(scale_to_tablet(value, extent) <= 0x7FFF);
            }
        }
    }

    #[test]
    fn unset_sinks_swallow_events() {
        let broker = InputBroker::default();
        broker.key_event(true, 0x61);
        broker.pointer_event(1, 100, 200);
    }

    #[test]
    fn keyboard_events_reach_sink() {
        let broker = InputBroker::default();
        let keyboard = Arc::new(RecordingKeyboard::default());
        broker.set_keyboard(keyboard.clone());

        broker.key_event(true, 0xff0d);
        broker.key_event(false, 0xff0d);

        assert_eq!(keyboard.events(), vec![(true, 0xff0d), (false, 0xff0d)]);
    }

    #[test]
    fn replacing_keyboard_replaces_sink() {
        let broker = InputBroker::default();
        let first = Arc::new(RecordingKeyboard::default());
        let second = Arc::new(RecordingKeyboard::default());
        broker.set_keyboard(first.clone());
        broker.set_keyboard(second.clone());

        broker.key_event(true, 0x61);

        assert!(first.events().is_empty());
        assert_eq!(second.events(), vec![(true, 0x61)]);
    }
}
