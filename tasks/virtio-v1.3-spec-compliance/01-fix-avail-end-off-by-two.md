# 01 — Fix avail_end off-by-2 in queue address calculation

**Spec:** Section 2.7 — Available ring size = flags(2) + idx(2) + ring[N](2*N) + used_event(2) = **6 + 2*N bytes**
**Severity:** Low (page alignment absorbs the error)
**Effort:** 1 line

## Problem

`queue.rs:164-165`:
```rust
let avail_end = self.avail_addr + 4 + u64::from(self.size) * 2;
```

Uses `4` where it should be `6`. The `used_event` field (2 bytes at the end of the available ring) is not accounted for in the size calculation. The used ring address is page-aligned after `avail_end`, so the 2-byte error is absorbed — but the formula is wrong per spec.

## File to modify

`crates/vmm-devices/src/virtio/queue.rs` — `set_addr_legacy()` at line 164

## Change

```rust
// Before:
let avail_end = self.avail_addr + 4 + u64::from(self.size) * 2;

// After:
let avail_end = self.avail_addr + 6 + u64::from(self.size) * 2;
```

Update the comment at line 160 to match:
```rust
// Available ring follows descriptors: 2 (flags) + 2 (idx) + 2*size (ring) + 2 (used_event)
```

## Test

The existing test `set_addr_legacy_computes_regions` at `queue.rs:503` verifies region addresses. After this change, the `used_addr` value should remain the same (page alignment absorbs the diff), so existing tests should still pass. Add a comment in the test noting that avail_end includes used_event.

Run: `cargo test -p vmm-devices -- queue`
