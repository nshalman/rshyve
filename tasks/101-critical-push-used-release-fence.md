# BUG: Missing Release fence in push_used()

**Priority:** Critical (spec violation)
**File:** `crates/vmm-devices/src/virtio/queue.rs` lines 247-276

## Problem

`push_used()` writes the used ring entry (lines 262-268) then writes the updated used
index (lines 272-275) with no Release fence between them. The guest reads the index
first, then reads entry data -- without the fence, it could see the updated index before
the entry data is visible on weak-ordering architectures.

`VirtioCompletion::write_used_entry()` at line 624 correctly has the fence.
`push_used()` does not. Currently affects VirtioRng (default `notify_queue` path).

## Fix

Add between line 268 (end of entry write) and line 272 (index write):

```rust
        }  // end of entry write block
    }

    // Release fence: guest must see entry data before updated index.
    // Matches VirtioCompletion::write_used_entry().
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);

    // Increment the used index
    let new_used_idx = used_idx.wrapping_add(1);
```

## Verification

- `cargo test -p vmm_devices` passes
- VirtioRng still works in guest (read from /dev/random)
- Code review confirms fence placement matches `VirtioCompletion::write_used_entry()`
