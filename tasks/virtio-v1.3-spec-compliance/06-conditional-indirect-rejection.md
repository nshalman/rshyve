# 06 — Gate indirect descriptor rejection on feature negotiation

**Spec:** Section 2.7.5.3 — If VIRTIO_F_INDIRECT_DESC is negotiated, device MUST handle indirect descriptors. If not negotiated, guest MUST NOT use them (and device should reject).
**Severity:** Low (no backend offers the feature today, so no current violation)
**Effort:** ~10 lines

## Problem

`queue.rs:407-412`:
```rust
// Reject indirect descriptors -- not supported.
if desc.is_indirect() {
    return None;
}
```

This rejection is unconditional — it doesn't check whether `VIRTIO_F_INDIRECT_DESC` was negotiated. If any backend ever offers this feature, the device would accept the negotiation but then reject the descriptors, violating the spec.

## File to modify

`crates/vmm-devices/src/virtio/queue.rs`

## Changes

### Step 1: Add `indirect_supported` field to VirtQueue

```rust
pub struct VirtQueue {
    size: u16,
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,
    last_avail_idx: u16,
    event_idx: bool,
    indirect_supported: bool,  // NEW
}
```

Initialize to `false` in `new()`. Add setter:

```rust
pub fn set_indirect_supported(&mut self, supported: bool) {
    self.indirect_supported = supported;
}
```

### Step 2: Update collect_chain to check the flag

```rust
if desc.is_indirect() {
    if !self.indirect_supported {
        // Feature not negotiated — guest is misbehaving
        return None;
    }
    // TODO: Walk indirect descriptor table when support is implemented.
    // For now, reject even if negotiated (feature not yet implemented).
    return None;
}
```

### Step 3: Wire up in pci.rs (GUEST_FEATURES handler)

After the existing event_idx propagation (line 361-364):

```rust
let indirect = accepted & bits::VIRTIO_F_RING_INDIRECT_DESC as u32 != 0;
for q in &mut vs.queues {
    q.set_event_idx(event_idx);
    q.set_indirect_supported(indirect);
}
```

## Test

```rust
#[test]
fn indirect_rejected_when_not_negotiated() {
    // Create queue with indirect_supported = false
    // Submit a descriptor with VRING_DESC_F_INDIRECT set
    // collect_chain should return None
}
```

Run: `cargo test -p vmm-devices -- queue`
