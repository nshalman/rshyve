# 21 — RING_EVENT_IDX disabled — 10-100x VM exits left on table

**Priority:** MEDIUM
**File:** `crates/vmm-devices/src/virtio/pci.rs:296-300`
**Category:** Performance — notification suppression disabled

## Problem

```rust
// TODO: RING_EVENT_IDX notification suppression is
// implemented but causes boot hangs due to a race...
// Disabled until the race is resolved.
self.device.device_features()
```

`VIRTIO_F_RING_EVENT_IDX` is fully implemented in `VirtQueue` and `VirtioCompletion` but disabled in feature advertisement. This feature lets:
- The guest batch multiple descriptor submissions into a single VM exit (kick suppression)
- The device batch multiple completions into a single interrupt (interrupt coalescing)

Without it, every descriptor added by the guest triggers a VM exit, and every completion triggers an interrupt. For sequential I/O at full queue depth (256 entries), this is potentially 256 VM exits + 256 interrupts vs. 1 + 1.

Commit `b4053cf` disabled this with the message "pending race fix."

## Fix

### Root cause investigation

The race is likely in the synchronization between:
1. Guest writes to `avail_event` (device-side notification threshold)
2. Device reads `avail_event` after processing the avail ring
3. Guest checks `used_event` before deciding whether to kick

The `update_used_event` in `queue.rs:233-240` and the `needs_notify` check in `queue.rs:586-608` implement the spec correctly, but the barrier ordering may be wrong.

### Likely fix

In `VirtioCompletion::write_used_entry` (line 624), the `fence(Ordering::Release)` ensures the used ring entry is visible before the index update. But the `needs_notify` check at line 594-608 reads `avail_event` AFTER writing the used index. This requires an additional `fence(Ordering::SeqCst)` between the used index write and the avail_event read to prevent the CPU from reordering them:

```rust
// Write used index
sub.write::<u16>(&new_used_idx);

// Full barrier: ensure the guest sees our used index update
// before we read the guest's avail_event value.
std::sync::atomic::fence(Ordering::SeqCst);

// Now read avail_event to decide if we should notify
let avail_event = /* read from guest memory */;
```

### Testing

1. Boot a Linux guest with `RING_EVENT_IDX` re-enabled
2. Run `fio --ioengine=libaio --iodepth=32 --bs=4k --rw=randread` — verify no hangs
3. Monitor VM exit rate — should drop significantly vs. disabled
4. Test with both virtio-blk and virtio-rng
