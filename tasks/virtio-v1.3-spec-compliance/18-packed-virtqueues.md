# 18 — Packed virtqueue support

**Spec:** Section 2.8 — Packed virtqueues (introduced in virtio 1.1, refined in 1.3).
**Severity:** Medium (performance feature — better cache behavior, modern-only)
**Effort:** ~400 lines (largest single task)

## Problem

The implementation only supports split virtqueues (`queue.rs`). Split queues use three separate memory regions — descriptor table, available ring, and used ring — which causes cache thrashing under high IOPS because the device bounces between three disjoint cache lines per I/O.

Packed virtqueues replace all three regions with a single descriptor ring. Both driver and device write to the same ring, using wrap counters and flag bits to distinguish ownership. Benefits:

1. **Cache locality**: one ring instead of three. The "hot" data (current descriptor) is always in the same cache line.
2. **Fewer memory barriers**: the wrap counter avoids the separate avail/used index fence dance.
3. **In-order completion**: common case (sequential I/O) needs no ID lookup — just advance the used pointer.

Packed queues are the modern replacement for split queues. Modern guests (Linux 5.0+ with `virtio_pci_modern`) prefer packed queues when advertised.

## Spec rules

1. Feature bit `VIRTIO_F_RING_PACKED` (bit 34) advertises packed queue support.
2. The driver and device negotiate packed vs. split during feature negotiation. If the driver does not accept `VIRTIO_F_RING_PACKED`, fall back to split.
3. Packed descriptor flags use `VRING_PACKED_DESC_F_AVAIL` (bit 7) and `VRING_PACKED_DESC_F_USED` (bit 15) to track ownership. The driver and device each maintain a wrap counter that flips when the ring wraps around.
4. A descriptor is "available" when its avail flag matches the driver's wrap counter and its used flag does NOT match the device's wrap counter.
5. The device marks a descriptor as "used" by setting both avail and used flags to match its wrap counter, then advancing its internal pointer.
6. `VIRTQ_DESC_F_NEXT` (bit 1) chains descriptors within a request. The last descriptor in a chain does not have NEXT set.
7. `VIRTQ_DESC_F_WRITE` (bit 2) marks device-writable buffers, same as split.
8. Indirect descriptors (`VIRTQ_DESC_F_INDIRECT`, bit 3) point to a packed indirect table in guest memory — same concept as split, but the table itself uses packed descriptor format.
9. The queue address is configured as a single 64-bit `desc` address (no separate avail/used addresses since there is only one ring).
10. Event suppression uses `Driver Event` and `Device Event` structures instead of the split `avail_event`/`used_event` fields.

## Design outline

### New file: `crates/vmm-devices/src/virtio/packed_queue.rs`

```rust
use std::sync::atomic::{fence, Ordering};
use vmm_core::mem::PhysMap;

/// Packed virtqueue descriptor (16 bytes, same size as split).
///
/// Spec 2.8.4: the ring is a contiguous array of these.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PackedDesc {
    /// Guest physical address of the buffer.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Descriptor ID (head index of the chain).
    pub id: u16,
    /// Flags: NEXT, WRITE, INDIRECT, AVAIL, USED.
    pub flags: u16,
}

/// Flag bits for packed descriptors.
const PACKED_F_NEXT: u16 = 1;
const PACKED_F_WRITE: u16 = 2;
const PACKED_F_INDIRECT: u16 = 4;
const PACKED_F_AVAIL: u16 = 1 << 7;
const PACKED_F_USED: u16 = 1 << 15;

/// Event suppression structure (4 bytes).
///
/// Spec 2.8.10: placed after the descriptor ring.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
struct PackedEventSuppress {
    /// desc_event_off (15:0) | desc_event_wrap (bit 15) | desc_event_flags (1:0)
    desc: u16,
    flags: u16,
}

/// Packed virtqueue state.
pub struct PackedVirtQueue {
    /// Queue size (number of descriptors).
    size: u16,
    /// Guest physical address of the descriptor ring.
    desc_addr: u64,
    /// Guest physical address of the Driver Event suppression structure.
    driver_event_addr: u64,
    /// Guest physical address of the Device Event suppression structure.
    device_event_addr: u64,
    /// Device's current position in the ring.
    next_avail: u16,
    /// Device's wrap counter (toggled on ring wrap).
    wrap_counter: bool,
}

impl PackedVirtQueue {
    pub fn new(size: u16) -> Self {
        assert!(size > 0 && size.is_power_of_two());
        Self {
            size,
            desc_addr: 0,
            driver_event_addr: 0,
            device_event_addr: 0,
            next_avail: 0,
            wrap_counter: true,
        }
    }

    /// Configure queue addresses (modern transport only).
    ///
    /// The descriptor ring starts at `desc_addr`. The driver and device
    /// event suppression structures follow immediately after the ring.
    pub fn set_addr(&mut self, desc_addr: u64) {
        self.desc_addr = desc_addr;
        let ring_bytes = u64::from(self.size) * 16;
        // Driver Event structure immediately after the ring (4 bytes)
        self.driver_event_addr = desc_addr + ring_bytes;
        // Device Event structure after Driver Event (4 bytes, aligned)
        self.device_event_addr = self.driver_event_addr + 4;
        self.next_avail = 0;
        self.wrap_counter = true;
    }

    pub fn is_configured(&self) -> bool {
        self.desc_addr != 0
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    /// Read a packed descriptor from the ring.
    fn read_desc(&self, physmap: &PhysMap, idx: u16) -> Option<PackedDesc> {
        let gpa = self.desc_addr + u64::from(idx) * 16;
        let sub = physmap.lookup(gpa, 16)?;
        sub.read::<PackedDesc>().ok()
    }

    /// Check if the descriptor at `next_avail` is available for the device.
    fn is_desc_avail(&self, desc: &PackedDesc) -> bool {
        let avail = (desc.flags & PACKED_F_AVAIL) != 0;
        let used = (desc.flags & PACKED_F_USED) != 0;
        avail == self.wrap_counter && used != self.wrap_counter
    }

    /// Pop the next available descriptor chain.
    ///
    /// Returns `(head_id, Vec<ChainBuf>)` or None if no descriptors
    /// are available. Advances `next_avail` and wraps as needed.
    pub fn pop_avail(
        &mut self,
        physmap: &PhysMap,
    ) -> Option<(u16, Vec<super::queue::ChainBuf>)> {
        if !self.is_configured() {
            return None;
        }

        let desc = self.read_desc(physmap, self.next_avail)?;

        // Acquire fence: see descriptor data written by driver.
        fence(Ordering::Acquire);

        if !self.is_desc_avail(&desc) {
            return None;
        }

        let head_id = desc.id;
        let mut bufs = Vec::new();
        let mut count = 0u16;

        // Walk the chain (may be a single descriptor or NEXT-chained).
        let mut idx = self.next_avail;
        let mut d = desc;
        loop {
            if count >= self.size {
                return None; // Malicious guest: chain exceeds ring
            }
            count += 1;

            if d.flags & PACKED_F_WRITE != 0 {
                bufs.push(super::queue::ChainBuf::Writable {
                    addr: d.addr,
                    len: d.len,
                });
            } else {
                bufs.push(super::queue::ChainBuf::Readable {
                    addr: d.addr,
                    len: d.len,
                });
            }

            // Advance ring position
            self.advance_avail();

            if d.flags & PACKED_F_NEXT == 0 {
                break;
            }

            idx = self.next_avail;
            d = self.read_desc(physmap, idx)?;
        }

        Some((head_id, bufs))
    }

    /// Mark a completed chain as "used" by writing flags back.
    ///
    /// For in-order completion, write the flags on the head descriptor
    /// with both AVAIL and USED matching the device's wrap counter,
    /// then set `len` to the number of bytes written.
    pub fn push_used(
        &self,
        physmap: &PhysMap,
        head_idx: u16,
        id: u16,
        len: u32,
        wrap: bool,
    ) {
        let gpa = self.desc_addr + u64::from(head_idx) * 16;
        if let Some(sub) = physmap.lookup(gpa, 16) {
            // Build flags: both AVAIL and USED match wrap counter
            let mut flags: u16 = 0;
            if wrap {
                flags |= PACKED_F_AVAIL | PACKED_F_USED;
            }
            // Write len at offset 8, id at offset 12, flags at offset 14
            if let Some(len_sub) = sub.subregion(8, 4) {
                let _ = len_sub.write::<u32>(&len);
            }
            if let Some(id_sub) = sub.subregion(12, 2) {
                let _ = id_sub.write::<u16>(&id);
            }

            // Release fence: ensure len/id visible before flags update
            fence(Ordering::Release);

            if let Some(flags_sub) = sub.subregion(14, 2) {
                let _ = flags_sub.write::<u16>(&flags);
            }
        }
    }

    /// Advance `next_avail`, wrapping the counter at ring end.
    fn advance_avail(&mut self) {
        self.next_avail += 1;
        if self.next_avail >= self.size {
            self.next_avail = 0;
            self.wrap_counter = !self.wrap_counter;
        }
    }

    pub fn reset(&mut self) {
        self.desc_addr = 0;
        self.driver_event_addr = 0;
        self.device_event_addr = 0;
        self.next_avail = 0;
        self.wrap_counter = true;
    }
}
```

### bits.rs — add constants

```rust
/// Device supports packed virtqueues (virtio 1.1+).
pub const VIRTIO_F_RING_PACKED: u64 = 1 << 34;
```

### mod.rs — register the new module

```rust
pub mod packed_queue;
pub use packed_queue::PackedVirtQueue;
```

### Integration with VirtioPciDevice (modern transport)

The modern transport (task 15) must check which queue type was negotiated:

```rust
// In VirtioModernPciDevice (from task 15):
enum QueueType {
    Split(VirtQueue),
    Packed(PackedVirtQueue),
}

// During FEATURES_OK: check if VIRTIO_F_RING_PACKED is negotiated
fn finalize_features(&mut self, features: u64) {
    self.use_packed = features & bits::VIRTIO_F_RING_PACKED != 0;
}

// Queue setup: create the appropriate queue type
fn enable_queue(&mut self, idx: u16) {
    if self.use_packed {
        let mut pq = PackedVirtQueue::new(self.queue_size);
        pq.set_addr(self.queue_desc);
        self.queues[idx] = QueueType::Packed(pq);
    } else {
        // ... split queue setup ...
    }
}
```

### VirtioDevice trait — optional packed queue support

No changes to the `VirtioDevice` trait are required. The packed queue produces the same `Vec<ChainBuf>` that `collect_chain()` produces for split queues. Device backends (block, rng, net) are queue-format agnostic — they only see `ChainBuf` slices.

## Files to modify

1. `crates/vmm-devices/src/virtio/packed_queue.rs` — **new file**, `PackedVirtQueue` implementation (~300 lines)
2. `crates/vmm-devices/src/virtio/bits.rs` — add `VIRTIO_F_RING_PACKED` and packed descriptor flag constants
3. `crates/vmm-devices/src/virtio/mod.rs` — add `pub mod packed_queue`
4. `crates/vmm-devices/src/virtio/pci_modern.rs` — (from task 15) select queue type based on negotiated features (~50 lines)
5. `crates/vmm-devices/src/virtio/queue.rs` — extract `ChainBuf` to a shared location or re-export (packed queue reuses it)

## Dependencies

- **Task 14** (u64 features) — `VIRTIO_F_RING_PACKED` is bit 34, which requires u64 feature words.
- **Task 15** (modern PCI transport) — packed queues use 64-bit queue addresses configured via the modern common config structure. They cannot be used with the legacy transport (which only has a 32-bit PFN register for queue addresses).
- Packed queues are independent of task 16 (MSI-X) and task 17 (block MQ), but all four combine well: modern transport + MSI-X + MQ + packed queues is the full virtio 1.3 data path.

## Test

```rust
#[test]
fn packed_queue_new_defaults() {
    let pq = PackedVirtQueue::new(256);
    assert_eq!(pq.size(), 256);
    assert!(!pq.is_configured());
}

#[test]
fn packed_queue_set_addr() {
    let mut pq = PackedVirtQueue::new(256);
    pq.set_addr(0x10_0000);
    assert!(pq.is_configured());
    // Driver Event at desc_addr + 256*16 = 0x10_0000 + 0x1000
    // Device Event at Driver Event + 4
}

#[test]
fn packed_desc_avail_flag_check() {
    // Descriptor with AVAIL=1, USED=0, wrap_counter=true → available
    let desc = PackedDesc {
        addr: 0x2000, len: 512, id: 0,
        flags: PACKED_F_AVAIL,  // avail=1, used=0
    };
    let pq = PackedVirtQueue::new(256);
    // wrap_counter starts as true; avail=true matches, used=false doesn't → avail
    assert!(pq.is_desc_avail(&desc));
}

#[test]
fn packed_desc_used_after_completion() {
    // After push_used, both AVAIL and USED flags match wrap counter
    // Descriptor should no longer be "available" to the device
}

#[test]
fn packed_wrap_counter_toggles() {
    let mut pq = PackedVirtQueue::new(4); // tiny ring for test
    pq.set_addr(0x1000);
    assert!(pq.wrap_counter); // starts true
    // Advance 4 times to wrap
    for _ in 0..4 {
        pq.advance_avail();
    }
    assert!(!pq.wrap_counter); // should have toggled
    assert_eq!(pq.next_avail, 0); // back to start
}

#[test]
fn packed_chain_produces_chain_bufs() {
    // Set up a 2-descriptor chain in guest memory:
    //   desc[0]: readable, NEXT set
    //   desc[1]: writable, no NEXT
    // pop_avail should return both as ChainBuf entries
}

#[test]
fn packed_chain_length_bounded() {
    // Set up a chain that loops (malicious guest)
    // pop_avail should return None after queue_size iterations
}

#[test]
#[should_panic(expected = "power of 2")]
fn packed_non_power_of_two_panics() {
    let _ = PackedVirtQueue::new(100);
}

#[test]
fn packed_reset_clears_state() {
    let mut pq = PackedVirtQueue::new(64);
    pq.set_addr(0x5000);
    pq.reset();
    assert!(!pq.is_configured());
}
```

Run: `cargo test -p vmm-devices -- packed`
