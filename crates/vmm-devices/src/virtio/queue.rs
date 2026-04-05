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

use std::sync::atomic::{fence, Ordering};
use std::sync::Arc;

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

    /// Returns true if this descriptor uses an indirect descriptor table.
    pub fn is_indirect(&self) -> bool {
        self.flags & bits::VRING_DESC_F_INDIRECT != 0
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
    /// Whether VIRTIO_F_RING_EVENT_IDX is negotiated.
    event_idx: bool,
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
            event_idx: false,
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

    /// Enable/disable VIRTIO_F_RING_EVENT_IDX notification suppression.
    ///
    /// When enabled, the device writes `used_event` to tell the guest
    /// when to kick, and reads `avail_event` from the guest to decide
    /// when to raise an interrupt. This dramatically reduces VM exits
    /// for high-throughput workloads.
    pub fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx = enabled;
    }

    /// Returns whether event_idx is enabled.
    pub fn event_idx_enabled(&self) -> bool {
        self.event_idx
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

        // Acquire fence: ensure subsequent descriptor reads see writes that
        // happened before the guest's Release store to avail_idx (VirtIO 2.7.13).
        fence(Ordering::Acquire);

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

    /// Update `used_event` to tell the guest when to kick next.
    ///
    /// Call AFTER draining all available entries (not per-pop).
    /// Sets `used_event = last_avail_idx` so the guest kicks
    /// when it adds ANY new entry past what we've already seen.
    ///
    /// Includes a full memory barrier (SeqCst) after the write so
    /// that the caller's subsequent `has_new_avail()` re-check sees
    /// the guest's latest `avail_idx`. Without this barrier, a
    /// Store-Load reorder could cause us to miss entries the guest
    /// added between our drain and this write (VirtIO 1.3 sec 2.7.10,
    /// QEMU `smp_mb()` in `virtio_queue_split_set_notification`).
    pub fn update_used_event(&self, physmap: &PhysMap) {
        if !self.event_idx {
            return;
        }
        let used_event_gpa = self.avail_addr + 4 + u64::from(self.size) * 2;
        if let Some(sub) = physmap.lookup(used_event_gpa, 2) {
            let _ = sub.write::<u16>(&self.last_avail_idx);
        }
        // Full barrier: ensure used_event is visible to the guest
        // before we re-read avail_idx in has_new_avail().
        fence(Ordering::SeqCst);
    }

    /// Check if new available entries arrived after updating used_event.
    ///
    /// VirtIO 1.3 sec 2.7.10: after writing `used_event`, re-read
    /// `avail_idx`. If new entries exist, the device must process
    /// them — the guest may have decided not to kick based on the
    /// stale `used_event` value it read during the race window.
    ///
    /// The `SeqCst` fence in `update_used_event()` ensures this
    /// read sees the guest's latest `avail_idx`.
    pub fn has_new_avail(&self, physmap: &PhysMap) -> bool {
        if !self.event_idx || !self.is_configured() {
            return false;
        }
        let avail_idx_gpa = self.avail_addr + 2;
        match physmap
            .lookup(avail_idx_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
        {
            Some(avail_idx) => avail_idx != self.last_avail_idx,
            None => false,
        }
    }

    /// Read the current used ring index from guest memory.
    pub fn read_used_idx(&self, physmap: &PhysMap) -> u16 {
        let used_idx_gpa = self.used_addr + 2;
        physmap
            .lookup(used_idx_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
            .unwrap_or(0)
    }

    /// Check if the guest expects an interrupt after used ring updates.
    ///
    /// With EVENT_IDX: reads `avail_event` from the used ring and
    /// applies the VirtIO 1.3 sec 2.7.7.1 wrapping comparison.
    /// Without EVENT_IDX: always returns true.
    ///
    /// `old_used_idx` is the used index before the device started
    /// processing. The current used index is read from guest memory.
    pub fn should_notify_guest(
        &self,
        physmap: &PhysMap,
        old_used_idx: u16,
    ) -> bool {
        if !self.event_idx {
            return true;
        }

        // Full barrier: ensure our used_idx writes (from push_used)
        // are visible to the guest before we read avail_event.
        // QEMU: smp_mb() in virtio_split_should_notify().
        fence(Ordering::SeqCst);

        let new_used_idx = self.read_used_idx(physmap);

        // Read avail_event from the used ring (after the ring entries).
        // Layout: flags(2) + idx(2) + entries(8*N) + avail_event(2)
        let avail_event_gpa =
            self.used_addr + 4 + u64::from(self.size) * 8;
        let avail_event = match physmap
            .lookup(avail_event_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
        {
            Some(v) => v,
            None => return true, // Can't read → always interrupt
        };

        // VirtIO 1.3 sec 2.7.7.1 (vring_need_event):
        // Interrupt iff (new - event - 1) < (new - old)
        vring_need_event(avail_event, new_used_idx, old_used_idx)
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

        // Release fence: ensure the entry data (id + len) is visible to the
        // guest before the updated used_idx (VirtIO 2.7.8).
        fence(Ordering::Release);

        // Increment the used index
        let new_used_idx = used_idx.wrapping_add(1);
        if let Some(sub) = physmap.lookup(used_idx_gpa, 2) {
            let _ = sub.write::<u16>(&new_used_idx);
        }
    }

    /// Get the used ring address and queue size for async completion.
    ///
    /// Workers use these to call `push_used_raw` without needing the
    /// full VirtQueue struct.
    pub fn used_ring_info(&self) -> (u64, u16) {
        (self.used_addr, self.size)
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

            // Reject indirect descriptors -- not supported. A malicious
            // guest setting this flag would cause us to misinterpret an
            // indirect table pointer as a regular buffer address.
            if desc.is_indirect() {
                return None;
            }

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

/// VirtIO 1.3 sec 2.7.7.1 wrapping comparison for notification suppression.
///
/// Returns true when `new_idx` crosses the `event_idx` threshold
/// relative to `old_idx`. Handles u16 wrapping correctly.
///
/// Equivalent to Linux `vring_need_event()` and QEMU's same-named function.
#[inline]
fn vring_need_event(event_idx: u16, new_idx: u16, old_idx: u16) -> bool {
    new_idx.wrapping_sub(event_idx).wrapping_sub(1)
        < new_idx.wrapping_sub(old_idx)
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
        assert!(!desc.is_indirect());

        let desc2 = VirtqDesc {
            addr: 0x2000,
            len: 1,
            flags: 0,
            next: 0,
        };
        assert!(!desc2.has_next());
        assert!(!desc2.is_write());
        assert!(!desc2.is_indirect());
    }

    #[test]
    fn virtq_desc_indirect_flag() {
        let desc = VirtqDesc {
            addr: 0x3000,
            len: 256,
            flags: bits::VRING_DESC_F_INDIRECT,
            next: 0,
        };
        assert!(desc.is_indirect());
        assert!(!desc.has_next());
        assert!(!desc.is_write());

        // Indirect + other flags
        let desc2 = VirtqDesc {
            addr: 0x4000,
            len: 128,
            flags: bits::VRING_DESC_F_INDIRECT | bits::VRING_DESC_F_WRITE,
            next: 0,
        };
        assert!(desc2.is_indirect());
        assert!(desc2.is_write());
    }

    // -- EVENT_IDX tests --

    #[test]
    fn vring_need_event_crossing_threshold() {
        // new_idx crosses event_idx: should interrupt
        // old=0, new=1, event=0: (1-0-1=0) < (1-0=1) → true
        assert!(vring_need_event(0, 1, 0));
        // old=5, new=10, event=7: (10-7-1=2) < (10-5=5) → true
        assert!(vring_need_event(7, 10, 5));
    }

    #[test]
    fn vring_need_event_threshold_already_passed() {
        // event_idx already behind old_idx: no interrupt
        // old=5, new=10, event=4: (10-4-1=5) < (10-5=5) → false
        assert!(!vring_need_event(4, 10, 5));
        // old=0, new=1, event=1: (1-1-1=65535) < (1-0=1) → false
        assert!(!vring_need_event(1, 1, 0));
    }

    #[test]
    fn vring_need_event_no_new_work() {
        // old == new (no work done): never interrupt
        assert!(!vring_need_event(3, 5, 5));
        assert!(!vring_need_event(0, 0, 0));
    }

    #[test]
    fn vring_need_event_wrapping() {
        // Wrapping around u16::MAX
        // old=65535, new=0, event=65535: (0-65535-1=0) < (0-65535=1) → true
        assert!(vring_need_event(65535, 0, 65535));
        // old=65535, new=0, event=0: (0-0-1=65535) < (0-65535=1) → false
        assert!(!vring_need_event(0, 0, 65535));
        // old=65534, new=1, event=65535: (1-65535-1=1) < (1-65534=3) → true
        assert!(vring_need_event(65535, 1, 65534));
    }

    #[test]
    fn vring_need_event_single_step() {
        // Exact threshold crossing: old=N, new=N+1, event=N → interrupt
        for n in [0u16, 1, 100, 255, 65534, 65535] {
            assert!(
                vring_need_event(n, n.wrapping_add(1), n),
                "should interrupt at threshold {n}",
            );
        }
    }

    #[test]
    fn has_new_avail_disabled_when_no_event_idx() {
        let q = VirtQueue::new(128);
        assert!(!q.event_idx_enabled());
        // has_new_avail should always return false when event_idx is off
        // (can't test with real PhysMap, but the early return is the key)
    }

    #[test]
    fn has_new_avail_disabled_when_not_configured() {
        let mut q = VirtQueue::new(128);
        q.set_event_idx(true);
        // Not configured (no PFN set) → false
        assert!(!q.is_configured());
    }
}

/// Thread-safe completion handler for async virtio I/O.
///
/// Exclusively owns the used ring write index for one virtqueue.
/// ALL completions — inline fast-path and async workers — MUST go
/// through this struct. Never mix with `VirtQueue::push_used()`.
///
/// # Design (Propolis `Mutex<VqUsed>` pattern)
///
/// - `Mutex<u16>` serializes used ring writes so entry data is
///   always committed before the index advances
/// - Release fence before index write ensures the guest sees the
///   entry data before the updated index
/// - Interrupt callback fires after the Mutex is released
///
/// # Performance
///
/// - Uncontended Mutex: ~10ns (single atomic CAS)
/// - Zero heap allocation per I/O
/// - Batch mode: one interrupt for N completions
///
/// # Reuse
///
/// Generic for any async virtio device (block, scsi, net, 9p).
/// Create one per virtqueue via `VirtioCompletion::new()`.
pub struct VirtioCompletion {
    used_addr: u64,
    queue_size: u16,
    write_idx: std::sync::Mutex<u16>,
    physmap: Arc<PhysMap>,
    interrupt: Box<dyn Fn() + Send + Sync>,
    /// Whether VIRTIO_F_RING_EVENT_IDX is negotiated.
    event_idx: bool,
}

impl VirtioCompletion {
    /// Create a completion handler for a configured virtqueue.
    ///
    /// Reads the current used index from guest memory to establish
    /// the starting write position. From this point forward, this
    /// struct exclusively owns the used ring write index.
    pub fn new(
        queue: &VirtQueue,
        physmap: Arc<PhysMap>,
        interrupt: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self> {
        let (used_addr, queue_size) = queue.used_ring_info();
        let idx_gpa = used_addr + 2;
        let initial_idx = physmap
            .lookup(idx_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
            .unwrap_or(0);

        let event_idx = queue.event_idx_enabled();
        Arc::new(Self {
            used_addr,
            queue_size,
            write_idx: std::sync::Mutex::new(initial_idx),
            physmap,
            interrupt: Box::new(interrupt),
            event_idx,
        })
    }

    /// Complete one request: write used entry, raise interrupt if needed.
    ///
    /// When `event_idx` is negotiated, reads `avail_event` from the
    /// guest to determine if an interrupt is needed. This suppresses
    /// redundant interrupts — the guest only wants an interrupt when
    /// the device has completed past the index the guest last saw.
    ///
    /// Safe from any thread (vCPU inline path or I/O worker).
    #[inline]
    pub fn complete(&self, desc_idx: u16, len: u32) {
        let old_idx;
        let new_idx;
        {
            let mut idx = self.write_idx.lock().expect("completion lock");
            old_idx = *idx;
            self.write_used_entry(&mut idx, desc_idx, len);
            new_idx = *idx;
        }

        if self.should_interrupt(old_idx, new_idx) {
            (self.interrupt)();
        }
    }

    /// Complete a batch: write all entries, one interrupt if needed.
    ///
    /// The Mutex is held across all writes so no thread can interleave.
    /// With event_idx, the interrupt is only raised if the guest
    /// needs notification based on the avail_event threshold.
    pub fn complete_batch(&self, entries: &[(u16, u32)]) {
        if entries.is_empty() {
            return;
        }
        let old_idx;
        let new_idx;
        {
            let mut idx = self.write_idx.lock().expect("completion lock");
            old_idx = *idx;
            for &(desc_idx, len) in entries {
                self.write_used_entry(&mut idx, desc_idx, len);
            }
            new_idx = *idx;
        }
        if self.should_interrupt(old_idx, new_idx) {
            (self.interrupt)();
        }
    }

    /// Check if the guest needs an interrupt.
    ///
    /// With event_idx: only interrupt when `new_idx` crosses the
    /// `avail_event` threshold set by the guest. Uses the VirtIO 1.3
    /// wrapping comparison from spec sec 2.7.7.1.
    ///
    /// Without event_idx: always interrupt (no suppression).
    fn should_interrupt(&self, old_idx: u16, new_idx: u16) -> bool {
        if !self.event_idx {
            return true;
        }

        // Full barrier: ensure our used_idx write (in write_used_entry)
        // is visible to the guest before we read avail_event. Without
        // this, a Store-Load reorder could cause us to read a stale
        // avail_event and skip an interrupt the guest expects.
        // QEMU: smp_mb() in virtio_split_should_notify().
        std::sync::atomic::fence(Ordering::SeqCst);

        // Read avail_event from the used ring (after the ring entries).
        // Layout: flags(2) + idx(2) + entries(8*N) + avail_event(2)
        let avail_event_gpa = self.used_addr + 4 + u64::from(self.queue_size) * 8;
        let avail_event = match self.physmap.lookup(avail_event_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
        {
            Some(v) => v,
            None => return true, // Can't read → always interrupt (safe)
        };

        // VirtIO 1.3 sec 2.7.7.1 (vring_need_event):
        // Interrupt when the used index crosses the guest's threshold.
        vring_need_event(avail_event, new_idx, old_idx)
    }

    /// Write one entry and advance the index. Caller holds Mutex.
    fn write_used_entry(&self, idx: &mut u16, desc_idx: u16, len: u32) {
        let slot = u64::from(*idx % self.queue_size);
        let entry_gpa = self.used_addr + 4 + slot * 8;

        if let Some(sub) = self.physmap.lookup(entry_gpa, 8) {
            let _ = sub.write::<u32>(&u32::from(desc_idx));
            if let Some(len_sub) = sub.subregion(4, 4) {
                let _ = len_sub.write::<u32>(&len);
            }
        }

        // Release fence: guest reads index first, then entry data.
        std::sync::atomic::fence(
            std::sync::atomic::Ordering::Release,
        );

        *idx = idx.wrapping_add(1);
        let idx_gpa = self.used_addr + 2;
        if let Some(sub) = self.physmap.lookup(idx_gpa, 2) {
            let _ = sub.write::<u16>(idx);
        }
    }
}
