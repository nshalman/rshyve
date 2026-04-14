# BUG: No INDIRECT descriptor rejection in collect_chain()

**Priority:** Critical (silent data corruption from non-compliant guests)
**File:** `crates/vmm-devices/src/virtio/queue.rs` lines 291-330

## Problem

`collect_chain()` walks descriptor chains checking `VRING_DESC_F_NEXT` and
`VRING_DESC_F_WRITE` but never checks `VRING_DESC_F_INDIRECT` (defined in bits.rs:36).

If a non-compliant guest sends indirect descriptors, the `addr` field (which points to
an indirect descriptor table) is treated as a data buffer address, causing silent data
corruption or I/O errors.

The device does NOT advertise `VIRTIO_F_RING_INDIRECT_DESC`, so a spec-compliant guest
will never send these. This is a defensive check.

## Fix

In `collect_chain()`, after `let desc = self.read_desc(physmap, idx)?;` (line 307), add:

```rust
let desc = self.read_desc(physmap, idx)?;
count += 1;

// Reject indirect descriptors -- device does not advertise
// VIRTIO_F_RING_INDIRECT_DESC.
if desc.flags & bits::VRING_DESC_F_INDIRECT != 0 {
    return None;
}

if desc.is_write() {
```

Callers already treat `None` as an error and skip the request or return IOERR.

## Verification

- `cargo test -p vmm_devices` passes
- Boot Linux guest, confirm I/O works (Linux won't use indirect without the feature bit)
- Optional: unit test creating a descriptor with INDIRECT flag, verify `collect_chain()` returns None
