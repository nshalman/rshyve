// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A display and input sinks the tests can drive and inspect.

use std::io;
use std::sync::Mutex;

use vmm_devices::display::{CursorShape, DisplaySource, Rect};
use vmm_devices::{KeyboardSink, PointerSink};

/// A display whose size, pixels and damage the test sets.
pub struct FakeDisplay {
    pub size: Mutex<(u32, u32)>,
    pub damage: Mutex<Option<Rect>>,
    pub requests: Mutex<Vec<(u32, u32)>>,
    /// The pointer's serial and shape, as the test sets them.
    pub cursor: Mutex<(u64, CursorShape)>,
    /// Sizes to report one per call before `size` again: a display
    /// resized between two reads.
    pub resizes: Mutex<std::collections::VecDeque<(u32, u32)>>,
    /// Rectangles painted one value, the last on top.
    pub painted: Mutex<Vec<(Rect, u32)>>,
    /// Every pace a server asked for, in order.
    pub paced: Mutex<Vec<Option<std::time::Duration>>>,
    /// Servers to wake on a change.
    pub wakers: vmm_devices::display::Wakers,
}

impl FakeDisplay {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            size: Mutex::new((width, height)),
            damage: Mutex::default(),
            requests: Mutex::default(),
            cursor: Mutex::new((0, CursorShape::Default)),
            resizes: Mutex::default(),
            painted: Mutex::default(),
            paced: Mutex::default(),
            wakers: Default::default(),
        }
    }

    /// The pixel value at (x, y): its index, so a test can tell where a
    /// copied pixel came from.
    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        let inside = |r: &Rect| {
            (r.x..r.x + r.width).contains(&x)
                && (r.y..r.y + r.height).contains(&y)
        };
        self.painted
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(r, _)| inside(r))
            .map_or_else(|| y * self.size.lock().unwrap().0 + x, |&(_, v)| v)
    }

    /// Paint `r` with `value` and report the whole display damaged, as a
    /// page-flipping guest does whatever changed.
    pub fn paint(&self, r: Rect, value: u32) {
        self.painted.lock().unwrap().push((r, value));
        let (width, height) = *self.size.lock().unwrap();
        *self.damage.lock().unwrap() = Some(Rect {
            x: 0,
            y: 0,
            width,
            height,
        });
        self.wakers.wake();
    }

    /// Set the guest's pointer, as a device does, waking servers.
    pub fn set_cursor(&self, serial: u64, shape: CursorShape) {
        *self.cursor.lock().unwrap() = (serial, shape);
        self.wakers.wake();
    }
}

impl DisplaySource for FakeDisplay {
    fn size(&self) -> (u32, u32) {
        match self.resizes.lock().unwrap().pop_front() {
            Some(size) => {
                *self.size.lock().unwrap() = size;
                size
            }
            None => *self.size.lock().unwrap(),
        }
    }

    fn take_damage(&self) -> Option<Rect> {
        self.damage.lock().unwrap().take()
    }

    fn read_rect(&self, r: Rect, dst: &mut [u8]) -> io::Result<()> {
        assert_eq!(dst.len(), (r.width * r.height * 4) as usize);
        for y in 0..r.height {
            for x in 0..r.width {
                let at = ((y * r.width + x) * 4) as usize;
                dst[at..at + 4].copy_from_slice(
                    &self.pixel(r.x + x, r.y + y).to_le_bytes(),
                );
            }
        }
        Ok(())
    }

    fn request_size(&self, width: u32, height: u32) -> bool {
        self.requests.lock().unwrap().push((width, height));
        true
    }

    fn cursor_serial(&self) -> u64 {
        self.cursor.lock().unwrap().0
    }

    fn cursor(&self) -> CursorShape {
        self.cursor.lock().unwrap().1.clone()
    }

    fn on_change(
        &self,
        waker: std::sync::Weak<dyn vmm_devices::display::DisplayWaker>,
    ) {
        self.wakers.add(waker);
    }

    fn pace(&self, interval: Option<std::time::Duration>) {
        self.paced.lock().unwrap().push(interval);
    }
}

#[derive(Default)]
pub struct Recorder {
    pub scancodes: Mutex<Vec<(bool, u8, bool)>>,
    pub pointer: Mutex<Vec<(u8, u16, u16)>>,
}

impl KeyboardSink for Recorder {
    fn key_event(&self, _down: bool, _keysym: u32) {
        panic!("RDP delivers scan codes, not keysyms");
    }

    fn scancode_event(&self, down: bool, code: u8, extended: bool) {
        self.scancodes.lock().unwrap().push((down, code, extended));
    }
}

impl PointerSink for Recorder {
    fn pointer_event(&self, buttons: u8, x: u16, y: u16) {
        self.pointer.lock().unwrap().push((buttons, x, y));
    }
}
