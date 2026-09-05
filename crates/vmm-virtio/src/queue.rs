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

use slog::debug;

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

/// One of the three rings a split virtqueue is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ring {
    Desc,
    Avail,
    Used,
}

impl Ring {
    /// The alignment VirtIO 1.3 sec 2.7 fixes for the ring.
    const fn align(self) -> u64 {
        match self {
            Ring::Desc => 16,
            Ring::Avail => 2,
            Ring::Used => 4,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Ring::Desc => "desc",
            Ring::Avail => "avail",
            Ring::Used => "used",
        }
    }
}

/// Why the device refuses the ring addresses a guest programmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAddrError {
    /// The address is zero, so the guest never programmed the ring.
    Unset(Ring),
    /// The address breaks the alignment the spec fixes for the ring.
    Misaligned { ring: Ring, addr: u64 },
    /// The ring is not mapped guest memory for its whole size.
    Unmapped { ring: Ring, addr: u64, len: usize },
}

impl std::fmt::Display for QueueAddrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unset(ring) => {
                write!(f, "{} ring address is zero", ring.name())
            }
            Self::Misaligned { ring, addr } => write!(
                f,
                "{} ring at {addr:#x} is not aligned to {}",
                ring.name(),
                ring.align(),
            ),
            Self::Unmapped { ring, addr, len } => write!(
                f,
                "{} ring at {addr:#x} is not {len} bytes of guest memory",
                ring.name(),
            ),
        }
    }
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
    /// Device-side shadow of the used ring index.
    ///
    /// Tracking this locally prevents TOCTOU attacks where a malicious
    /// guest modifies the used_idx in shared memory between our read
    /// and write, causing entries to be placed at wrong ring slots.
    shadow_used_idx: u16,
    /// Whether VIRTIO_F_RING_EVENT_IDX is negotiated.
    event_idx: bool,
    /// Whether VIRTIO_F_RING_INDIRECT_DESC is negotiated.
    indirect_supported: bool,
    /// The address check, kept from the first use of the rings.
    addrs_ok: Option<bool>,
    /// Where a refusal is reported. Silent until a transport sets one.
    log: Option<slog::Logger>,
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
        assert!(
            size > 0 && size.is_power_of_two(),
            "queue size must be power of 2"
        );
        assert!(size <= MAX_QUEUE_SIZE, "queue size exceeds maximum");
        Self {
            size,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            last_avail_idx: 0,
            shadow_used_idx: 0,
            event_idx: false,
            indirect_supported: false,
            addrs_ok: None,
            log: None,
        }
    }

    /// Report refusals through `log`.
    pub fn set_log(&mut self, log: slog::Logger) {
        self.log = Some(log);
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
        // The feature adds the suppression word to what the device
        // reads, so a cached verdict was taken against a shorter ring.
        if self.event_idx != enabled {
            self.addrs_ok = None;
        }
        self.event_idx = enabled;
    }

    /// Returns whether event_idx is enabled.
    pub fn event_idx_enabled(&self) -> bool {
        self.event_idx
    }

    /// Returns whether indirect descriptor support is enabled.
    pub fn indirect_supported(&self) -> bool {
        self.indirect_supported
    }

    /// Enable/disable indirect descriptor support.
    pub fn set_indirect_supported(&mut self, supported: bool) {
        self.indirect_supported = supported;
    }

    /// Return the guest physical address of the available ring.
    pub fn avail_addr(&self) -> u64 {
        self.avail_addr
    }

    /// Read the avail ring idx from guest memory.
    ///
    /// Returns the guest driver's avail_idx which may be ahead of the
    /// kernel's cached value (the kernel may have consumed all pending
    /// buffers). Used during migration export to capture the true
    /// number of available buffers.
    pub fn read_avail_idx(&self, physmap: &PhysMap) -> u16 {
        if self.avail_addr == 0 {
            return 0;
        }
        let gpa = self.avail_addr + 2;
        physmap
            .lookup(gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
            .unwrap_or(0)
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
        self.addrs_ok = None;
        if pfn == 0 {
            self.desc_addr = 0;
            self.avail_addr = 0;
            self.used_addr = 0;
            self.last_avail_idx = 0;
            self.shadow_used_idx = 0;
            return;
        }

        let base = u64::from(pfn) * PAGE_SIZE as u64;

        // Descriptor table: size * sizeof(VirtqDesc) = size * 16
        self.desc_addr = base;

        // Available ring follows descriptors: 2 (flags) + 2 (idx) + 2*size (ring)
        self.avail_addr = base + u64::from(self.size) * 16;

        // Used ring is page-aligned after the available ring
        // avail: flags(2) + idx(2) + ring(2*size) + used_event(2)
        let avail_end = self.avail_addr + 6 + u64::from(self.size) * 2;
        self.used_addr = align_up(avail_end, PAGE_SIZE as u64);
    }

    /// Set queue addresses directly (modern transport).
    ///
    /// Modern (VirtIO 1.0+) transport provides separate addresses for
    /// the descriptor table, available ring, and used ring instead of
    /// computing them from a single PFN.
    pub fn set_addr_modern(&mut self, desc: u64, avail: u64, used: u64) {
        self.addrs_ok = None;
        self.desc_addr = desc;
        self.avail_addr = avail;
        self.used_addr = used;
        self.last_avail_idx = 0;
        self.shadow_used_idx = 0;
    }

    /// Set the last consumed avail index (for migration restore).
    pub fn set_last_avail_idx(&mut self, idx: u16) {
        self.last_avail_idx = idx;
    }

    /// Set the shadow used index (for migration restore).
    pub fn set_shadow_used_idx(&mut self, idx: u16) {
        self.shadow_used_idx = idx;
    }

    /// Read the used ring idx from guest memory.
    ///
    /// For devices using VirtioCompletion (like virtio-blk), the
    /// VirtQueue's shadow_used_idx is never updated. This reads the
    /// actual used_ring->idx that VirtioCompletion wrote to guest memory.
    pub fn read_used_ring_idx(&self, physmap: &PhysMap) -> u16 {
        if self.used_addr == 0 {
            return 0;
        }
        let gpa = self.used_addr + 2;
        physmap
            .lookup(gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
            .unwrap_or(0)
    }

    /// Get the descriptor table address.
    pub fn desc_addr(&self) -> u64 {
        self.desc_addr
    }

    /// Get the used ring address.
    pub fn used_addr(&self) -> u64 {
        self.used_addr
    }

    /// Get the last available ring index processed by the device.
    pub fn last_avail_idx(&self) -> u16 {
        self.last_avail_idx
    }

    /// Get the device-side shadow of the used ring index.
    pub fn shadow_used_idx(&self) -> u16 {
        self.shadow_used_idx
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
        self.addrs_ok = None;
        self.desc_addr = 0;
        self.avail_addr = 0;
        self.used_addr = 0;
        self.last_avail_idx = 0;
        self.shadow_used_idx = 0;
    }

    /// Bytes the descriptor table occupies: 16 per entry (sec 2.7.5).
    fn desc_table_bytes(&self) -> usize {
        usize::from(self.size) * 16
    }

    /// Bytes of the available ring the device can reach: flags, idx and
    /// one 2 byte entry per descriptor (sec 2.7.6).
    fn avail_ring_bytes(&self) -> usize {
        4 + usize::from(self.size) * 2 + self.event_field_bytes()
    }

    /// Bytes of the used ring the device can reach: flags, idx and one
    /// 8 byte entry per descriptor (sec 2.7.8).
    fn used_ring_bytes(&self) -> usize {
        4 + usize::from(self.size) * 8 + self.event_field_bytes()
    }

    /// The suppression field each ring carries only under EVENT_IDX.
    /// Demanding it otherwise would refuse a ring that works, because
    /// the device never reads that far without the feature.
    fn event_field_bytes(&self) -> usize {
        if self.event_idx {
            2
        } else {
            0
        }
    }

    /// Check the three ring addresses against the guest memory map.
    ///
    /// A ring that is misaligned, or that leaves mapped memory, fails
    /// every access the device makes on it. Nothing else notices: the
    /// device keeps taking heads off the available ring and completing
    /// none of them, while the guest still reads DRIVER_OK.
    pub fn validate_addrs(
        &self,
        physmap: &PhysMap,
    ) -> Result<(), QueueAddrError> {
        for (ring, addr, len) in [
            (Ring::Desc, self.desc_addr, self.desc_table_bytes()),
            (Ring::Avail, self.avail_addr, self.avail_ring_bytes()),
            (Ring::Used, self.used_addr, self.used_ring_bytes()),
        ] {
            if addr == 0 {
                return Err(QueueAddrError::Unset(ring));
            }
            if !addr.is_multiple_of(ring.align()) {
                return Err(QueueAddrError::Misaligned { ring, addr });
            }
            // `lookup` refuses a range that wraps or that leaves the
            // mapped run, which is the reach of every later access.
            if physmap.lookup(addr, len).is_none() {
                return Err(QueueAddrError::Unmapped { ring, addr, len });
            }
        }
        Ok(())
    }

    /// Whether the rings passed [`Self::validate_addrs`], checked once
    /// and remembered.
    ///
    /// The verdict holds until the guest programs the rings again. A
    /// queue that failed cannot start working, and rechecking it on
    /// every kick would let a guest drive the log.
    fn addrs_usable(&mut self, physmap: &PhysMap) -> bool {
        if let Some(ok) = self.addrs_ok {
            return ok;
        }
        match self.validate_addrs(physmap) {
            Ok(()) => {
                self.addrs_ok = Some(true);
                true
            }
            Err(error) => {
                self.addrs_ok = Some(false);
                if let Some(log) = &self.log {
                    debug!(log, "virtqueue refused"; "reason" => %error);
                }
                false
            }
        }
    }

    /// Whether the queue refused the addresses the guest programmed.
    ///
    /// The transport turns this into DEVICE_NEEDS_RESET. A driver has
    /// no other way to learn that the device stopped.
    pub fn needs_reset(&self) -> bool {
        self.addrs_ok == Some(false)
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
        // The rings are checked here, not in the address setters: only
        // a caller with the memory map can say where they land.
        if !self.addrs_usable(physmap) {
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
        let ring_offset = u64::from(self.last_avail_idx % self.size) * 2;
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

    /// Write `avail_event` in the used ring to suppress guest kicks.
    ///
    /// Call AFTER draining all available entries (not per-pop).
    /// Sets `avail_event = last_avail_idx` so the guest kicks
    /// when it adds ANY new entry past what we've already seen.
    ///
    /// VirtIO 1.3 sec 2.7.7: `avail_event` is in the used ring
    /// (device→driver), at offset: flags(2) + idx(2) + entries(8*N).
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
        // avail_event is at end of used ring: used_addr + 4 + size*8
        let avail_event_gpa = self.used_addr + 4 + u64::from(self.size) * 8;
        if let Some(sub) = physmap.lookup(avail_event_gpa, 2) {
            let _ = sub.write::<u16>(&self.last_avail_idx);
        }
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

    /// Return the current used ring index from the device-side shadow.
    ///
    /// Does not read from guest memory — uses the trusted shadow value
    /// to prevent a malicious guest from influencing notification decisions.
    pub fn read_used_idx(&self, _physmap: &PhysMap) -> u16 {
        self.shadow_used_idx
    }

    /// Check if the guest expects an interrupt after used ring updates.
    ///
    /// With EVENT_IDX: reads `used_event` from the avail ring (the
    /// driver's threshold) and applies the VirtIO 1.3 sec 2.7.7.1
    /// wrapping comparison.
    /// Without EVENT_IDX: checks VIRTQ_AVAIL_F_NO_INTERRUPT flag.
    ///
    /// `old_used_idx` is the used index before the device started
    /// processing. The current used index is read from guest memory.
    pub fn should_notify_guest(
        &self,
        physmap: &PhysMap,
        old_used_idx: u16,
    ) -> bool {
        if !self.event_idx {
            // Without EVENT_IDX, check the avail ring flags field.
            // If VIRTQ_AVAIL_F_NO_INTERRUPT is set, the driver doesn't
            // want used-buffer notifications right now.
            if self.avail_addr != 0 {
                if let Some(sub) = physmap.lookup(self.avail_addr, 2) {
                    if let Ok(flags) = sub.read::<u16>() {
                        return flags & bits::VIRTQ_AVAIL_F_NO_INTERRUPT == 0;
                    }
                }
            }
            return true;
        }

        // Full barrier: ensure our used_idx writes (from push_used)
        // are visible to the guest before we read used_event.
        // QEMU: smp_mb() in virtio_split_should_notify().
        fence(Ordering::SeqCst);

        let new_used_idx = self.read_used_idx(physmap);

        // Read used_event from the avail ring (driver's threshold).
        // Layout: flags(2) + idx(2) + ring(2*N) + used_event(2)
        let used_event_gpa = self.avail_addr + 4 + u64::from(self.size) * 2;
        let used_event = match physmap
            .lookup(used_event_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
        {
            Some(v) => v,
            None => return true, // Can't read → always interrupt
        };

        // VirtIO 1.3 sec 2.7.7.1 (vring_need_event):
        // Interrupt iff (new - event - 1) < (new - old)
        vring_need_event(used_event, new_used_idx, old_used_idx)
    }

    /// Push an entry onto the used ring.
    ///
    /// `desc_idx` is the head descriptor index, `len` is the total
    /// number of bytes written to device-writable buffers.
    ///
    /// Uses a device-side shadow of used_idx to prevent TOCTOU attacks
    /// where a malicious guest modifies the shared used_idx between
    /// our read and write.
    pub fn push_used(&mut self, physmap: &PhysMap, desc_idx: u16, len: u32) {
        let used_idx = self.shadow_used_idx;

        // Write the used ring entry: id (u32) + len (u32)
        let ring_offset = u64::from(used_idx % self.size) * 8;
        let entry_gpa = self.used_addr + 4 + ring_offset;
        if let Some(sub) = physmap.lookup(entry_gpa, 8) {
            let _ = sub.write::<u32>(&(u32::from(desc_idx)));
            if let Some(len_sub) = sub.subregion(4, 4) {
                let _ = len_sub.write::<u32>(&len);
            }
        }

        // Release fence: ensure the entry data (id + len) is visible to the
        // guest before the updated used_idx (VirtIO 2.7.8).
        fence(Ordering::Release);

        // Increment the shadow and write to guest memory
        self.shadow_used_idx = used_idx.wrapping_add(1);
        let used_idx_gpa = self.used_addr + 2;
        if let Some(sub) = physmap.lookup(used_idx_gpa, 2) {
            let _ = sub.write::<u16>(&self.shadow_used_idx);
        }
    }

    /// Get the used ring address and queue size for async completion.
    ///
    /// Workers use these to call `push_used_raw` without needing the
    /// full VirtQueue struct.
    pub fn used_ring_info(&self) -> (u64, u16) {
        (self.used_addr, self.size)
    }

    /// Walk an indirect descriptor table starting at `gpa` with `len` bytes.
    ///
    /// Each indirect descriptor is 16 bytes. The table contains
    /// `len / 16` descriptors chained via NEXT flags. INDIRECT flag
    /// inside the table is invalid (VirtIO 1.3 sec 2.7.5.3.1).
    fn walk_indirect_table(
        &self,
        physmap: &PhysMap,
        gpa: u64,
        len: u32,
    ) -> Option<Vec<ChainBuf>> {
        let num_descs = len as usize / 16;
        if num_descs == 0 || !(len as usize).is_multiple_of(16) {
            return None;
        }
        if num_descs > usize::from(self.size) {
            return None;
        }

        let mut bufs = Vec::with_capacity(num_descs);
        let mut idx: usize = 0;
        let mut iterations: usize = 0;

        loop {
            if idx >= num_descs || iterations >= num_descs {
                return None; // Out of bounds or cycle detected
            }
            iterations += 1;

            let desc_gpa = gpa.checked_add((idx as u64).checked_mul(16)?)?;
            let sub = physmap.lookup(desc_gpa, 16)?;
            let desc: VirtqDesc = sub.read::<VirtqDesc>().ok()?;

            // INDIRECT inside indirect is invalid
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
                idx = desc.next as usize;
            } else {
                break;
            }
        }

        Some(bufs)
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

            if desc.is_indirect() {
                if !self.indirect_supported {
                    return None;
                }
                // INDIRECT + NEXT is invalid (VirtIO 1.3 sec 2.7.5.3)
                if desc.has_next() {
                    return None;
                }
                return self.walk_indirect_table(physmap, desc.addr, desc.len);
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

mod completion;
pub use completion::VirtioCompletion;

#[cfg(test)]
mod tests;
