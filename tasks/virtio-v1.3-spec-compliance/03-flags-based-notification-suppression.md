# 03 — Add flags-based notification suppression (non-EVENT_IDX path)

**Spec:** Section 2.7.7 — Without EVENT_IDX: "If flags is 1, the device SHOULD NOT send a notification. If flags is 0, the device MUST send a notification."
**Severity:** High (spec violation, wastes VM exits)
**Effort:** ~20 lines across 2 locations

## Problem

When EVENT_IDX is NOT negotiated, both `should_notify_guest()` and `VirtioCompletion::should_interrupt()` return `true` unconditionally. The device never reads the available ring's `flags` field, so it always interrupts even when the guest explicitly set `VIRTQ_AVAIL_F_NO_INTERRUPT` (flags=1).

Linux guests use this flag during NAPI polling to suppress interrupts while draining the ring.

### Location 1: `queue.rs:303-309`
```rust
pub fn should_notify_guest(&self, physmap: &PhysMap, old_used_idx: u16) -> bool {
    if !self.event_idx {
        return true;  // BUG: always notifies
    }
    // EVENT_IDX path below is correct...
```

### Location 2: `queue.rs:792-794`
```rust
fn should_interrupt(&self, old_idx: u16, new_idx: u16) -> bool {
    if !self.event_idx {
        return true;  // BUG: always notifies
    }
    // EVENT_IDX path below is correct...
```

## Files to modify

`crates/vmm-devices/src/virtio/queue.rs`

## Change — Location 1 (VirtQueue::should_notify_guest, line ~308)

```rust
if !self.event_idx {
    // Spec 2.7.7: read flags from available ring.
    // VIRTQ_AVAIL_F_NO_INTERRUPT (bit 0) = suppress notifications.
    let flags = physmap.lookup(self.avail_addr, 2)
        .and_then(|sub| sub.read::<u16>().ok())
        .unwrap_or(0);
    return flags & 1 == 0;  // Notify only if NO_INTERRUPT is NOT set
}
```

## Change — Location 2 (VirtioCompletion::should_interrupt, line ~793)

`VirtioCompletion` stores `avail_addr` (set at construction from `queue.used_ring_info()`). But it currently only stores `used_addr` and `queue_size`. Need to also store `avail_addr`.

### Step 1: Add `avail_addr` to VirtioCompletion

In the `VirtioCompletion` struct (around line 700), add:
```rust
avail_addr: u64,
```

In `VirtioCompletion::new()` (around line 720), capture it:
```rust
let (used_addr, queue_size) = queue.used_ring_info();
let avail_addr = queue.avail_addr();  // need to add this getter
```

### Step 2: Add `avail_addr()` getter to VirtQueue

```rust
pub fn avail_addr(&self) -> u64 {
    self.avail_addr
}
```

### Step 3: Fix should_interrupt

```rust
fn should_interrupt(&self, old_idx: u16, new_idx: u16) -> bool {
    if !self.event_idx {
        // Spec 2.7.7: read flags from available ring.
        let flags = self.physmap.lookup(self.avail_addr, 2)
            .and_then(|sub| sub.read::<u16>().ok())
            .unwrap_or(0);
        return flags & 1 == 0;
    }
    // ... EVENT_IDX path unchanged ...
```

## Constants to add in bits.rs

```rust
/// Available ring: driver sets this to suppress used-buffer notifications.
pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
```

## Test

```rust
#[test]
fn notification_suppressed_by_flags() {
    // Set up a VirtQueue with event_idx=false
    // Write flags=1 to avail_addr in guest memory
    // Call should_notify_guest() → should return false
    // Write flags=0 → should return true
}
```

Run: `cargo test -p vmm-devices -- queue`
