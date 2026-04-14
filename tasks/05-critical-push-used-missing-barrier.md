# 05 — VirtQueue::push_used missing memory barrier

**Priority:** CRITICAL
**File:** `crates/vmm-devices/src/virtio/queue.rs:247-276`
**Category:** Correctness — data race, I/O corruption

## Problem

`push_used` writes the used ring entry (lines 262-268) and then updates the used index (lines 272-274) with NO memory barrier between them. The guest can see the incremented index before the entry data is visible.

Compare with `VirtioCompletion::write_used_entry` (line ~624) which correctly inserts:

```rust
std::sync::atomic::fence(Ordering::Release);
```

between the entry write and the index update. Direct callers of `push_used` (e.g., `VirtioRng`) do NOT get this barrier.

The virtio spec (section 2.7.7.2) requires: "The device MUST perform a suitable memory barrier before the used index update, to ensure the guest sees the most up-to-date used ring entry."

## Fix

Add a release fence between the entry write and the index update in `push_used`:

```rust
// Write the used ring entry: id (u32) + len (u32)
let ring_offset = u64::from(used_idx % self.size) * 8;
let entry_gpa = self.used_addr + 4 + ring_offset;
if let Some(sub) = physmap.lookup(entry_gpa, 8) {
    let _ = sub.write::<u32>(&(u32::from(desc_idx)));
    if let Some(len_sub) = sub.subregion(4, 4) {
        let _ = len_sub.write::<u32>(&len);
    }
}

// Barrier: ensure entry is visible before updating the index.
// Required by virtio spec section 2.7.7.2.
std::sync::atomic::fence(std::sync::atomic::Ordering::Release);

// Increment the used index
let new_used_idx = used_idx.wrapping_add(1);
if let Some(sub) = physmap.lookup(used_idx_gpa, 2) {
    let _ = sub.write::<u16>(&new_used_idx);
}
```

## Testing

This is hard to unit-test (requires observing memory ordering). Verify by:
1. Code inspection — confirm barrier is present
2. Stress-test with high-throughput virtio-rng reads under memory pressure
3. Run with ThreadSanitizer if possible
