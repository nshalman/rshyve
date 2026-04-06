// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Packed virtqueue implementation (VirtIO 1.3 spec section 2.8).
//!
//! In a packed virtqueue, all three functions (descriptor, available, used)
//! are merged into a single descriptor ring. Availability and usage are
//! indicated by flag bits within each descriptor rather than by separate
//! index-based rings. A wrap counter toggles each time the ring position
//! wraps around, distinguishing old entries from new ones.
//!
//! # Layout
//!
//! ```text
//! +----------------------------------+
//! | Descriptor Ring (16 * size)      |  desc_addr
//! +----------------------------------+
//! | Driver Event Suppression (4)     |  driver_event_addr
//! +----------------------------------+
//! | Device Event Suppression (4)     |  device_event_addr
//! +----------------------------------+
//! ```

use std::sync::atomic::{fence, Ordering};

use vmm_core::mem::PhysMap;

use super::queue::ChainBuf;

// ---------------------------------------------------------------------------
// Packed descriptor flags (VirtIO 1.3 sec 2.8.5)
// ---------------------------------------------------------------------------

/// Buffer continues via the next descriptor.
pub const PACKED_F_NEXT: u16 = 1;

/// Buffer is device-writable (otherwise device read-only).
pub const PACKED_F_WRITE: u16 = 2;

/// Buffer contains an indirect descriptor table.
pub const PACKED_F_INDIRECT: u16 = 4;

/// Descriptor available flag (set by driver to mark available).
pub const PACKED_F_AVAIL: u16 = 1 << 7;

/// Descriptor used flag (set by device to mark used).
pub const PACKED_F_USED: u16 = 1 << 15;

/// Size of one packed descriptor in bytes.
const DESC_SIZE: usize = 16;

// ---------------------------------------------------------------------------
// PackedDesc
// ---------------------------------------------------------------------------

/// A single packed virtqueue descriptor as laid out in guest memory.
///
/// VirtIO 1.3 sec 2.8.5 -- each descriptor is 16 bytes. Unlike split
/// virtqueues, there is no separate `next` index; chaining uses the
/// NEXT flag with implicit sequential ordering in the ring.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PackedDesc {
    /// Guest physical address of the buffer.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Buffer ID -- opaque identifier returned to the driver on completion.
    pub id: u16,
    /// Flags: NEXT, WRITE, INDIRECT, AVAIL, USED.
    pub flags: u16,
}

impl PackedDesc {
    /// Returns true if this descriptor chains to the next one.
    pub fn has_next(&self) -> bool {
        self.flags & PACKED_F_NEXT != 0
    }

    /// Returns true if this buffer is device-writable (guest reads from it).
    pub fn is_write(&self) -> bool {
        self.flags & PACKED_F_WRITE != 0
    }

    /// Returns true if this descriptor uses an indirect descriptor table.
    pub fn is_indirect(&self) -> bool {
        self.flags & PACKED_F_INDIRECT != 0
    }

    /// Returns the state of the AVAIL flag.
    pub fn avail_flag(&self) -> bool {
        self.flags & PACKED_F_AVAIL != 0
    }

    /// Returns the state of the USED flag.
    pub fn used_flag(&self) -> bool {
        self.flags & PACKED_F_USED != 0
    }
}

// ---------------------------------------------------------------------------
// PackedVirtQueue
// ---------------------------------------------------------------------------

/// A packed virtqueue (VirtIO 1.3 sec 2.8).
///
/// Tracks the single descriptor ring and the two event suppression
/// structures that follow it. The device maintains separate avail/used
/// positions with independent wrap counters.
pub struct PackedVirtQueue {
    /// Queue size (number of descriptors). Must be a power of 2.
    size: u16,
    /// Guest physical address of the descriptor ring.
    desc_addr: u64,
    /// Guest physical address of the driver event suppression structure.
    driver_event_addr: u64,
    /// Guest physical address of the device event suppression structure.
    device_event_addr: u64,

    // -- Available-side tracking (device reads from driver) --
    /// Next descriptor index to check for availability.
    next_avail: u16,
    /// Current wrap counter for the available side. Toggles each time
    /// `next_avail` wraps around the ring.
    wrap_counter: bool,

    // -- Used-side tracking (device writes back to driver) --
    /// Next descriptor index to write used status into.
    next_used: u16,
    /// Current wrap counter for the used side. Toggles each time
    /// `next_used` wraps around the ring.
    used_wrap_counter: bool,
}

impl PackedVirtQueue {
    /// Create a new packed virtqueue with the given size.
    ///
    /// The size must be a power of 2 and nonzero.
    ///
    /// # Panics
    ///
    /// Panics if `size` is 0 or not a power of 2.
    pub fn new(size: u16) -> Self {
        assert!(
            size > 0 && size.is_power_of_two(),
            "packed queue size must be a power of 2"
        );
        Self {
            size,
            desc_addr: 0,
            driver_event_addr: 0,
            device_event_addr: 0,
            next_avail: 0,
            wrap_counter: true,
            next_used: 0,
            used_wrap_counter: true,
        }
    }

    /// Set the descriptor ring base address.
    ///
    /// The driver and device event suppression structures immediately
    /// follow the descriptor ring in guest memory (VirtIO 1.3 sec 2.8.10):
    ///   driver_event_addr = desc_addr + size * 16
    ///   device_event_addr = driver_event_addr + 4
    pub fn set_addr(&mut self, desc_addr: u64) {
        self.desc_addr = desc_addr;
        let ring_bytes = u64::from(self.size) * DESC_SIZE as u64;
        self.driver_event_addr = desc_addr + ring_bytes;
        self.device_event_addr = self.driver_event_addr + 4;
    }

    /// Returns true if the queue has been configured with a valid address.
    pub fn is_configured(&self) -> bool {
        self.desc_addr != 0
    }

    /// Get the queue size (number of descriptors).
    pub fn size(&self) -> u16 {
        self.size
    }

    /// Read a descriptor from the ring at the given index.
    ///
    /// Returns `None` if `idx` is out of bounds or the GPA is unmapped.
    pub fn read_desc(&self, physmap: &PhysMap, idx: u16) -> Option<PackedDesc> {
        if idx >= self.size {
            return None;
        }
        let gpa = self.desc_addr + u64::from(idx) * DESC_SIZE as u64;
        let sub = physmap.lookup(gpa, DESC_SIZE)?;
        sub.read::<PackedDesc>().ok()
    }

    /// Check whether a descriptor is available for the device to process.
    ///
    /// VirtIO 1.3 sec 2.8.6: a descriptor is available when its AVAIL
    /// flag matches the current wrap counter and its USED flag does not.
    pub fn is_desc_avail(&self, desc: &PackedDesc) -> bool {
        desc.avail_flag() == self.wrap_counter
            && desc.used_flag() != self.wrap_counter
    }

    /// Pop the next available descriptor chain from the ring.
    ///
    /// Returns `(head_id, chain_bufs)` where `head_id` is the buffer ID
    /// from the head descriptor. Chains are formed by consecutive
    /// descriptors with the NEXT flag set (VirtIO 1.3 sec 2.8.6).
    ///
    /// Returns `None` if no descriptors are available or the queue is
    /// not configured.
    pub fn pop_avail(
        &mut self,
        physmap: &PhysMap,
    ) -> Option<(u16, Vec<ChainBuf>)> {
        if !self.is_configured() {
            return None;
        }

        let head_desc = self.read_desc(physmap, self.next_avail)?;
        if !self.is_desc_avail(&head_desc) {
            return None;
        }

        // Acquire fence: ensure we see the descriptor data the driver
        // wrote before it set the AVAIL flag (VirtIO 1.3 sec 2.8.13).
        fence(Ordering::Acquire);

        let head_id = head_desc.id;
        let mut bufs = Vec::new();
        let mut count = 0u16;

        // Walk the chain. In packed queues, chained descriptors are
        // sequential in the ring (no explicit `next` index).
        let mut desc = head_desc;
        loop {
            if count >= self.size {
                // Safety bound: prevent infinite loops from bad guests.
                return None;
            }
            count += 1;

            if desc.is_write() {
                bufs.push(ChainBuf::Writable {
                    addr: desc.addr,
                    len: desc.len,
                });
            } else {
                bufs.push(ChainBuf::Readable {
                    addr: desc.addr,
                    len: desc.len,
                });
            }

            if !desc.has_next() {
                self.advance_avail();
                break;
            }

            self.advance_avail();
            desc = self.read_desc(physmap, self.next_avail)?;
        }

        Some((head_id, bufs))
    }

    /// Mark a descriptor as used by writing AVAIL+USED flags.
    ///
    /// VirtIO 1.3 sec 2.8.8: the device signals completion by writing
    /// the descriptor with both AVAIL and USED flags set to match the
    /// `used_wrap_counter`. The `id` and `len` fields are written to
    /// the descriptor at the current `next_used` position.
    ///
    /// A Release fence ensures the buffer data the device wrote is
    /// visible to the driver before the flag update.
    pub fn push_used(
        &mut self,
        physmap: &PhysMap,
        head_idx: u16,
        id: u16,
        len: u32,
    ) {
        let _ = head_idx; // Available for future use (e.g., in-order completion)

        let gpa =
            self.desc_addr + u64::from(self.next_used) * DESC_SIZE as u64;

        // Build the flags: both AVAIL and USED match used_wrap_counter
        // to signal completion. No NEXT/WRITE/INDIRECT flags on used.
        let mut flags: u16 = 0;
        if self.used_wrap_counter {
            flags |= PACKED_F_AVAIL | PACKED_F_USED;
        }

        let used_desc = PackedDesc {
            addr: 0,
            len,
            id,
            flags,
        };

        // Release fence: ensure buffer data written by the device is
        // visible to the driver before the used flag update
        // (VirtIO 1.3 sec 2.8.13).
        fence(Ordering::Release);

        if let Some(sub) = physmap.lookup(gpa, DESC_SIZE) {
            // write to guest memory; no recovery if unmapped
            let _ = sub.write::<PackedDesc>(&used_desc);
        }

        self.advance_used();
    }

    /// Advance the available-side ring position.
    ///
    /// Increments `next_avail` and toggles `wrap_counter` when wrapping.
    pub fn advance_avail(&mut self) {
        self.next_avail += 1;
        if self.next_avail >= self.size {
            self.next_avail = 0;
            self.wrap_counter = !self.wrap_counter;
        }
    }

    /// Advance the used-side ring position.
    ///
    /// Increments `next_used` and toggles `used_wrap_counter` when wrapping.
    fn advance_used(&mut self) {
        self.next_used += 1;
        if self.next_used >= self.size {
            self.next_used = 0;
            self.used_wrap_counter = !self.used_wrap_counter;
        }
    }

    /// Reset the queue to its initial state.
    ///
    /// Clears all addresses and resets positions and wrap counters.
    pub fn reset(&mut self) {
        self.desc_addr = 0;
        self.driver_event_addr = 0;
        self.device_event_addr = 0;
        self.next_avail = 0;
        self.wrap_counter = true;
        self.next_used = 0;
        self.used_wrap_counter = true;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults() {
        let q = PackedVirtQueue::new(64);
        assert_eq!(q.size(), 64);
        assert_eq!(q.next_avail, 0);
        assert_eq!(q.next_used, 0);
        assert!(q.wrap_counter);
        assert!(q.used_wrap_counter);
        assert!(!q.is_configured());
    }

    #[test]
    fn set_addr_computes_event_addrs() {
        let mut q = PackedVirtQueue::new(128);
        let base = 0x1000_0000u64;
        q.set_addr(base);

        assert!(q.is_configured());
        assert_eq!(q.desc_addr, base);
        // driver_event follows the ring: base + 128 * 16 = base + 2048
        assert_eq!(q.driver_event_addr, base + 128 * 16);
        // device_event follows driver_event: + 4
        assert_eq!(q.device_event_addr, base + 128 * 16 + 4);
    }

    #[test]
    fn wrap_counter_toggles() {
        let mut q = PackedVirtQueue::new(4);
        assert!(q.wrap_counter);
        assert_eq!(q.next_avail, 0);

        // Advance through the full ring
        for _ in 0..4 {
            q.advance_avail();
        }
        // Should have wrapped: back to index 0, counter toggled
        assert_eq!(q.next_avail, 0);
        assert!(!q.wrap_counter);

        // Advance through again
        for _ in 0..4 {
            q.advance_avail();
        }
        assert_eq!(q.next_avail, 0);
        assert!(q.wrap_counter);
    }

    #[test]
    fn used_wrap_counter_toggles() {
        let mut q = PackedVirtQueue::new(4);
        assert!(q.used_wrap_counter);

        for _ in 0..4 {
            q.advance_used();
        }
        assert_eq!(q.next_used, 0);
        assert!(!q.used_wrap_counter);
    }

    #[test]
    fn reset_clears_state() {
        let mut q = PackedVirtQueue::new(16);
        q.set_addr(0x2000_0000);
        q.next_avail = 5;
        q.wrap_counter = false;
        q.next_used = 3;
        q.used_wrap_counter = false;

        q.reset();

        assert!(!q.is_configured());
        assert_eq!(q.desc_addr, 0);
        assert_eq!(q.driver_event_addr, 0);
        assert_eq!(q.device_event_addr, 0);
        assert_eq!(q.next_avail, 0);
        assert!(q.wrap_counter);
        assert_eq!(q.next_used, 0);
        assert!(q.used_wrap_counter);
    }

    #[test]
    #[should_panic(expected = "power of 2")]
    fn non_power_of_2_panics() {
        PackedVirtQueue::new(3);
    }

    #[test]
    #[should_panic(expected = "power of 2")]
    fn zero_size_panics() {
        PackedVirtQueue::new(0);
    }

    #[test]
    fn desc_flag_helpers() {
        let d = PackedDesc {
            addr: 0,
            len: 0,
            id: 0,
            flags: PACKED_F_NEXT | PACKED_F_WRITE | PACKED_F_AVAIL,
        };
        assert!(d.has_next());
        assert!(d.is_write());
        assert!(!d.is_indirect());
        assert!(d.avail_flag());
        assert!(!d.used_flag());
    }

    #[test]
    fn is_desc_avail_logic() {
        let q = PackedVirtQueue::new(16);
        // wrap_counter starts true

        // AVAIL=true, USED=false -> available
        let avail_desc = PackedDesc {
            addr: 0,
            len: 0,
            id: 0,
            flags: PACKED_F_AVAIL,
        };
        assert!(q.is_desc_avail(&avail_desc));

        // AVAIL=false, USED=false -> not available (AVAIL != wrap_counter)
        let not_avail = PackedDesc {
            addr: 0,
            len: 0,
            id: 0,
            flags: 0,
        };
        assert!(!q.is_desc_avail(&not_avail));

        // AVAIL=true, USED=true -> not available (USED == wrap_counter)
        let both_set = PackedDesc {
            addr: 0,
            len: 0,
            id: 0,
            flags: PACKED_F_AVAIL | PACKED_F_USED,
        };
        assert!(!q.is_desc_avail(&both_set));
    }

    #[test]
    fn advance_avail_partial() {
        let mut q = PackedVirtQueue::new(8);
        q.advance_avail();
        assert_eq!(q.next_avail, 1);
        assert!(q.wrap_counter); // No wrap yet

        q.advance_avail();
        q.advance_avail();
        assert_eq!(q.next_avail, 3);
        assert!(q.wrap_counter);
    }
}
