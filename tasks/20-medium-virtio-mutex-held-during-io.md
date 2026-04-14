# 20 — Virtio notify_queue holds Mutex across entire I/O operation

**Priority:** MEDIUM
**File:** `crates/vmm-devices/src/virtio/pci.rs:384-398`
**Category:** Performance — serialization bottleneck

## Problem

When the guest writes to `QUEUE_NOTIFY`, `bar_write` acquires the `virtio_state` Mutex and calls `self.device.notify_queue()` with the lock still held. For `VirtioBlock`, the inline single-request fast path does `pread`/`pwrite` syscalls while the Mutex is locked.

This means:
- No other vCPU can access any virtio register (ISR, device status, config) during disk I/O
- For HDD-backed images, the lock is held for milliseconds
- Multi-queue or multi-vCPU workloads serialize completely

## Fix

Extract queue data under the lock, drop the lock, then process:

```rust
bits::LEGACY_REG_QUEUE_NOTIFY => {
    let queue_idx = val as u16;
    let queue_snapshot = {
        let vs = self.virtio_state.lock().expect("virtio state");
        vs.queues.get(queue_idx as usize).cloned()
    };
    // Lock is dropped here
    if let Some(queue) = queue_snapshot {
        self.device.notify_queue(queue_idx, &queue, &self.physmap);
    }
}
```

This requires `VirtQueue` to be `Clone` or to extract just the addresses/sizes needed for processing. The `VirtioCompletion` pattern already works without holding the queue lock (it stores `used_addr` and `size` independently).

Alternative: Split `VirtioPciState` into separate locks for queues vs. config registers, so config reads don't block behind I/O.

## Impact

At 100K IOPS with 4KB random reads (NVMe-backed), each pread takes ~10us. The Mutex is held for 10us per I/O. With 2 vCPUs sending I/O, the second vCPU stalls for up to 10us per operation.
