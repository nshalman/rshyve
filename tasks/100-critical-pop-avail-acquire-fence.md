# BUG: Missing Acquire fence in pop_avail()

**Priority:** Critical (spec violation)
**File:** `crates/vmm-devices/src/virtio/queue.rs` line 204

## Problem

`pop_avail()` reads `avail_idx` from guest memory via `sub.read::<u16>()` (volatile read)
but has no Acquire fence after. VirtIO spec 2.7.13.1 states: "The device MUST perform a
suitable device memory barrier before reading the available ring descriptors."

Without the fence, descriptor reads at lines 214-216 could be reordered before the
`avail_idx` read on weak-ordering architectures. On x86 this is masked (all loads have
acquire semantics), but the spec mandates it.

## Fix

Add after line 204 (the `avail_idx` read), before line 207 (the comparison):

```rust
let avail_idx: u16 = sub.read::<u16>().ok()?;

// VirtIO spec 2.7.13.1: device memory barrier before reading descriptors.
std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);

// Check if there are new entries
if self.last_avail_idx == avail_idx {
```

## Verification

- `cargo test -p vmm_devices` passes
- No-op on x86 (generates no additional instructions)
- Correctness by construction against VirtIO 2.7.13.1
