// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The commands each I/O submission queue has admitted and not yet
//! finished.
//!
//! A tag stays taken until the guest can see the command's completion,
//! not only until the backend is done with it, so a guest that never
//! frees completion queue space cannot grow what the controller holds.

use super::queues::QueueInstance;

/// What the controller needs to finish a command it handed out.
#[derive(Clone, Copy)]
pub(super) struct Admitted {
    pub(super) serial: u64,
    pub(super) cid: u16,
    pub(super) opcode: u8,
    pub(super) cq_id: u16,
    pub(super) sq_instance: QueueInstance,
    pub(super) cq_instance: QueueInstance,
}

#[derive(Clone, Copy)]
enum Slot {
    Free,
    InFlight(Admitted),
    /// Finished, with its completion waiting for guest CQ space.
    Held,
}

#[derive(Default)]
pub(super) struct SqSlots {
    slots: Vec<Slot>,
    free: Vec<u16>,
    in_flight: u32,
    held: u32,
}

impl SqSlots {
    /// Commands the backend has not finished.
    pub(super) fn in_flight(&self) -> u32 {
        self.in_flight
    }

    /// Whether [`Self::admit`] would find a tag.
    pub(super) fn has_room(&self, depth: u16) -> bool {
        !self.free.is_empty() || self.slots.len() < usize::from(depth)
    }

    fn take_tag(&mut self, depth: u16, slot: Slot) -> Option<u16> {
        if let Some(tag) = self.free.pop() {
            self.slots[usize::from(tag)] = slot;
            return Some(tag);
        }
        let tag = u16::try_from(self.slots.len()).ok()?;
        if tag >= depth {
            return None;
        }
        self.slots.push(slot);
        Some(tag)
    }

    /// A tag below `depth` for a new command, or `None` when the queue
    /// already has `depth` commands the guest has not seen finish.
    pub(super) fn admit(&mut self, depth: u16, cmd: Admitted) -> Option<u16> {
        let tag = self.take_tag(depth, Slot::InFlight(cmd))?;
        self.in_flight += 1;
        Some(tag)
    }

    /// End the command in `tag`, if `serial` still names it.
    ///
    /// The tag stays taken: the caller gives it back with
    /// [`Self::release`] once the completion is written or dropped.
    pub(super) fn finish(&mut self, tag: u16, serial: u64) -> Option<Admitted> {
        let slot = self.slots.get_mut(usize::from(tag))?;
        let Slot::InFlight(cmd) = *slot else {
            return None;
        };
        if cmd.serial != serial {
            return None;
        }
        *slot = Slot::Held;
        self.in_flight -= 1;
        self.held += 1;
        Some(cmd)
    }

    /// Give back a tag whose completion is written or dropped.
    pub(super) fn release(&mut self, tag: u16) {
        let Some(slot) = self.slots.get_mut(usize::from(tag)) else {
            return;
        };
        if !matches!(slot, Slot::Held) {
            return;
        }
        *slot = Slot::Free;
        self.held -= 1;
        self.free.push(tag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(serial: u64) -> Admitted {
        Admitted {
            serial,
            cid: 7,
            opcode: 2,
            cq_id: 1,
            sq_instance: 1,
            cq_instance: 2,
        }
    }

    #[test]
    fn a_queue_admits_no_more_than_its_depth() {
        let mut slots = SqSlots::default();
        assert_eq!(slots.admit(2, cmd(1)), Some(0));
        assert_eq!(slots.admit(2, cmd(2)), Some(1));
        assert_eq!(slots.admit(2, cmd(3)), None);
        assert_eq!(slots.in_flight(), 2);
    }

    #[test]
    fn a_tag_comes_back_only_once_released() {
        let mut slots = SqSlots::default();
        let tag = slots.admit(1, cmd(1)).expect("tag");
        assert!(slots.finish(tag, 1).is_some());
        assert_eq!(slots.in_flight(), 0);
        assert_eq!(slots.admit(1, cmd(2)), None, "held still owns the tag");

        slots.release(tag);
        assert_eq!(slots.admit(1, cmd(2)), Some(tag));
    }

    #[test]
    fn a_stale_or_repeated_completion_finds_nothing() {
        let mut slots = SqSlots::default();
        let tag = slots.admit(4, cmd(9)).expect("tag");
        assert!(slots.finish(tag, 8).is_none(), "wrong serial");
        assert!(slots.finish(tag, 9).is_some());
        assert!(slots.finish(tag, 9).is_none(), "second completion");
        assert!(slots.finish(3, 9).is_none(), "tag never handed out");
        assert_eq!(slots.in_flight(), 0);
    }

    #[test]
    fn release_ignores_a_tag_that_is_not_held() {
        let mut slots = SqSlots::default();
        let tag = slots.admit(4, cmd(1)).expect("tag");
        slots.release(tag);
        assert_eq!(slots.in_flight(), 1, "released an in-flight tag");
        slots.release(40);
    }
}
