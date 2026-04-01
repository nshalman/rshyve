// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtQueue implementation -- the core virtio ring buffer.
//!
//! Implements the split virtqueue layout as defined in VirtIO 1.2 sec 2.7.
//! The queue consists of three regions in guest memory:
//! - Descriptor table: array of buffer descriptors
//! - Available ring: indices of descriptors the driver offers to the device
//! - Used ring: indices of descriptors the device has finished with
//!
//! # Safety
//!
//! All guest memory accesses go through [`PhysMap::lookup`] which
//! validates GPAs. Descriptor chains are bounded by queue size to
//! prevent infinite loops from malicious guests.

use vmm_core::common::PAGE_SIZE;
use vmm_core::mem::PhysMap;

use super::bits;

/// Maximum allowed queue size (VirtIO spec).
pub const MAX_QUEUE_SIZE: u16 = 32768;

/// A single virtqueue descriptor as laid out in guest memory.
///
/// VirtIO 1.2 sec 2.7.5 -- each descriptor is 16 bytes.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct VirtqDesc {
    /// Guest physical address of the buffer.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Descriptor flags (NEXT, WRITE, INDIRECT).
    pub flags: u16,
    /// Index of the next descriptor if NEXT is set.
    pub next: u16,
}

impl VirtqDesc {
    /// Returns true if this descriptor chains to another.
    pub fn has_next(&self) -> bool {
        self.flags & bits::VRING_DESC_F_NEXT != 0
    }

    /// Returns true if this buffer is device-writable (guest reads from it).
    pub fn is_write(&self) -> bool {
        self.flags & bits::VRING_DESC_F_WRITE != 0
    }
}

/// A buffer segment within a descriptor chain, classified by direction.
#[derive(Debug)]
pub enum ChainBuf {
    /// Device-readable buffer (guest wrote data for us to consume).
    Readable { addr: u64, len: u32 },
    /// Device-writable buffer (we write data for the guest to consume).
    Writable { addr: u64, len: u32 },
}

/// A split virtqueue.
///
/// Manages the descriptor table, available ring, and used ring for one
/// queue. The queue addresses are set by the guest during device init
/// via the legacy PCI transport (queue PFN register).
pub struct VirtQueue {
    /// Queue size (number of descriptors). Must be a power of 2.
    size: u16,
    /// Guest physical address of the descriptor table.
    desc_addr: u64,
    /// Guest physical address of the available ring.
    avail_addr: u64,
    /// Guest physical address of the used ring.
    used_addr: u64,
    /// Last available index we processed.
    last_avail_idx: u16,
}

impl VirtQueue {
    /// Create a new virtqueue with the given size.
    ///
    /// The size must be a power of 2 and at most [`MAX_QUEUE_SIZE`].
    ///
    /// # Panics
    ///
    /// Panics if `size` is 0, not a power of 2, or exceeds MAX_QUEUE_SIZE.
    pub fn new(size: u16) -> Self {
        assert!(size > 0 && size.is_power_of_two(), "queue size must be power of 2");
        assert!(size <= MAX_QUEUE_SIZE, "queue size exceeds maximum");
        Self {
            size,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            last_avail_idx: 0,
        }
    }

    /// Get the queue size (number of descriptors).
    pub fn size(&self) -> u16 {
        self.size
    }

    /// Returns true if the queue has been configured with valid addresses.
    pub fn is_configured(&self) -> bool {
        self.desc_addr != 0
    }

    /// Set the queue addresses from a legacy PFN (page frame number).
    ///
    /// In legacy mode, the guest writes a single PFN and the device
    /// calculates the three region addresses from it using the VirtIO
    /// 1.0 legacy layout:
    /// - Descriptors at PFN * PAGE_SIZE
    /// - Available ring at PFN * PAGE_SIZE + size * 16
    /// - Used ring aligned to next page boundary after available ring
    pub fn set_addr_legacy(&mut self, pfn: u32) {
        if pfn == 0 {
            self.desc_addr = 0;
            self.avail_addr = 0;
            self.used_addr = 0;
            self.last_avail_idx = 0;
            return;
        }

        let base = u64::from(pfn) * PAGE_SIZE as u64;

        // Descriptor table: size * sizeof(VirtqDesc) = size * 16
        self.desc_addr = base;

        // Available ring follows descriptors: 2 (flags) + 2 (idx) + 2*size (ring)
        self.avail_addr = base + u64::from(self.size) * 16;

        // Used ring is page-aligned after the available ring
        let avail_end =
            self.avail_addr + 4 + u64::from(self.size) * 2;
        self.used_addr = align_up(avail_end, PAGE_SIZE as u64);
    }

    /// Get the legacy PFN for this queue (desc_addr / PAGE_SIZE).
    pub fn pfn(&self) -> u32 {
        if self.desc_addr == 0 {
            0
        } else {
            (self.desc_addr / PAGE_SIZE as u64) as u32
        }
    }

    /// Reset the queue state (called on device reset).
    pub fn reset(&mut self) {
        self.desc_addr = 0;
        self.avail_addr = 0;
        self.used_addr = 0;
        self.last_avail_idx = 0;
    }

    /// Read a descriptor from the descriptor table.
    ///
    /// Returns `None` if `idx` is out of bounds or the GPA is unmapped.
    pub fn read_desc(&self, physmap: &PhysMap, idx: u16) -> Option<VirtqDesc> {
        if idx >= self.size {
            return None;
        }
        let offset = u64::from(idx) * 16;
        let gpa = self.desc_addr + offset;
        let sub = physmap.lookup(gpa, 16)?;
        sub.read::<VirtqDesc>().ok()
    }

    /// Pop the next available descriptor index from the available ring.
    ///
    /// Returns `None` if no new descriptors are available.
    pub fn pop_avail(&mut self, physmap: &PhysMap) -> Option<u16> {
        if !self.is_configured() {
            return None;
        }

        // Read the available ring index (at avail_addr + 2)
        let avail_idx_gpa = self.avail_addr + 2;
        let sub = physmap.lookup(avail_idx_gpa, 2)?;
        let avail_idx: u16 = sub.read::<u16>().ok()?;

        // Check if there are new entries
        if self.last_avail_idx == avail_idx {
            return None;
        }

        // Read the descriptor index from the ring
        let ring_offset =
            u64::from(self.last_avail_idx % self.size) * 2;
        let ring_entry_gpa = self.avail_addr + 4 + ring_offset;
        let sub = physmap.lookup(ring_entry_gpa, 2)?;
        let desc_idx: u16 = sub.read::<u16>().ok()?;

        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);

        // Validate the descriptor index
        if desc_idx >= self.size {
            return None;
        }

        Some(desc_idx)
    }

    /// Push an entry onto the used ring.
    ///
    /// `desc_idx` is the head descriptor index, `len` is the total
    /// number of bytes written to device-writable buffers.
    pub fn push_used(&self, physmap: &PhysMap, desc_idx: u16, len: u32) {
        // Read current used index
        let used_idx_gpa = self.used_addr + 2;
        let used_idx = match physmap.lookup(used_idx_gpa, 2) {
            Some(sub) => match sub.read::<u16>() {
                Ok(v) => v,
                Err(_) => return,
            },
            None => return,
        };

        // Write the used ring entry: id (u32) + len (u32)
        let ring_offset =
            u64::from(used_idx % self.size) * 8;
        let entry_gpa = self.used_addr + 4 + ring_offset;
        if let Some(sub) = physmap.lookup(entry_gpa, 8) {
            // Write id (descriptor index as u32)
            let _ = sub.write::<u32>(&(u32::from(desc_idx)));
            // Write len at offset 4
            if let Some(len_sub) = sub.subregion(4, 4) {
                let _ = len_sub.write::<u32>(&len);
            }
        }

        // Increment the used index
        let new_used_idx = used_idx.wrapping_add(1);
        if let Some(sub) = physmap.lookup(used_idx_gpa, 2) {
            let _ = sub.write::<u16>(&new_used_idx);
        }
    }

    /// Walk a descriptor chain starting from `head`, collecting buffers.
    ///
    /// The chain length is bounded by queue size to prevent infinite
    /// loops from malicious guests. Returns `None` if any descriptor
    /// in the chain cannot be read.
    pub fn collect_chain(
        &self,
        physmap: &PhysMap,
        head: u16,
    ) -> Option<Vec<ChainBuf>> {
        let mut bufs = Vec::new();
        let mut idx = head;
        let mut count = 0u16;

        loop {
            // Guard against infinite loops: a chain cannot have more
            // entries than the queue has descriptors.
            if count >= self.size {
                return None;
            }

            let desc = self.read_desc(physmap, idx)?;
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

            if desc.has_next() {
                idx = desc.next;
            } else {
                break;
            }
        }

        Some(bufs)
    }

    /// Process a descriptor chain, calling `handler` for each buffer.
    ///
    /// Returns `false` if the chain could not be walked (bad descriptor).
    pub fn process_chain(
        &self,
        physmap: &PhysMap,
        head: u16,
        mut handler: impl FnMut(&VirtqDesc),
    ) -> bool {
        let mut idx = head;
        let mut count = 0u16;

        loop {
            if count >= self.size {
                return false;
            }

            let desc = match self.read_desc(physmap, idx) {
                Some(d) => d,
                None => return false,
            };
            count += 1;

            handler(&desc);

            if desc.has_next() {
                idx = desc.next;
            } else {
                break;
            }
        }

        true
    }
}

/// Align `val` up to the next multiple of `align`.
/// `align` must be a power of 2.
fn align_up(val: u64, align: u64) -> u64 {
    (val + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_queue_defaults() {
        let q = VirtQueue::new(256);
        assert_eq!(q.size(), 256);
        assert!(!q.is_configured());
        assert_eq!(q.pfn(), 0);
    }

    #[test]
    fn set_addr_legacy_computes_regions() {
        let mut q = VirtQueue::new(256);
        // PFN 1 => base = 4096
        q.set_addr_legacy(1);
        assert!(q.is_configured());
        assert_eq!(q.pfn(), 1);

        // Desc at base
        assert_eq!(q.desc_addr, 4096);
        // Avail at base + 256 * 16 = base + 4096 = 8192
        assert_eq!(q.avail_addr, 4096 + 256 * 16);
        // Used at next page boundary after avail ring
        let avail_end = q.avail_addr + 4 + 256 * 2;
        let expected_used = align_up(avail_end, PAGE_SIZE as u64);
        assert_eq!(q.used_addr, expected_used);
    }

    #[test]
    fn set_addr_legacy_zero_resets() {
        let mut q = VirtQueue::new(128);
        q.set_addr_legacy(1);
        assert!(q.is_configured());

        q.set_addr_legacy(0);
        assert!(!q.is_configured());
        assert_eq!(q.desc_addr, 0);
        assert_eq!(q.avail_addr, 0);
        assert_eq!(q.used_addr, 0);
    }

    #[test]
    fn reset_clears_state() {
        let mut q = VirtQueue::new(64);
        q.set_addr_legacy(10);
        q.reset();
        assert!(!q.is_configured());
        assert_eq!(q.pfn(), 0);
    }

    #[test]
    #[should_panic(expected = "power of 2")]
    fn non_power_of_two_panics() {
        let _ = VirtQueue::new(100);
    }

    #[test]
    #[should_panic(expected = "power of 2")]
    fn zero_size_panics() {
        let _ = VirtQueue::new(0);
    }

    #[test]
    fn align_up_works() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
    }

    #[test]
    fn virtq_desc_flags() {
        let desc = VirtqDesc {
            addr: 0x1000,
            len: 512,
            flags: bits::VRING_DESC_F_NEXT | bits::VRING_DESC_F_WRITE,
            next: 1,
        };
        assert!(desc.has_next());
        assert!(desc.is_write());

        let desc2 = VirtqDesc {
            addr: 0x2000,
            len: 1,
            flags: 0,
            next: 0,
        };
        assert!(!desc2.has_next());
        assert!(!desc2.is_write());
    }
}
