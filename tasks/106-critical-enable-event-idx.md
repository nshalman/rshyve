# PERF: Enable EVENT_IDX notification suppression

**Priority:** Critical (biggest single performance win)
**Files:** `queue.rs`, `block.rs`, `mod.rs`, `pci.rs`

## Problem

`VIRTIO_F_RING_EVENT_IDX` is commented out at pci.rs:298 due to a boot hang race.
Without it, EVERY completion → interrupt → VM exit. At 100K IOPS = 100K VM exits/sec.
With EVENT_IDX, interrupts drop 10-100x.

## Root Cause of Boot Hang

Classic "check-then-act" race (VirtIO spec 2.7.13.3):

1. `pop_avail()` drains all available entries
2. `update_used_event()` writes `used_event = last_avail_idx`
3. Between steps 1 and 2, guest adds new entries
4. Guest sees `used_event`, decides no kick needed
5. Device never processes new entries → hang

## Fix: Re-check After Event Setting

### A. Add `has_new_avail()` to VirtQueue (queue.rs)

```rust
/// Check if new entries arrived after updating used_event.
///
/// VirtIO spec 2.7.13.3: after writing used_event, re-read
/// avail_idx. If new entries exist, process them -- the guest
/// may have decided not to kick based on the used_event value.
pub fn has_new_avail(&self, physmap: &PhysMap) -> bool {
    if !self.event_idx || !self.is_configured() {
        return false;
    }
    let avail_idx_gpa = self.avail_addr + 2;
    match physmap.lookup(avail_idx_gpa, 2)
        .and_then(|sub| sub.read::<u16>().ok())
    {
        Some(avail_idx) => avail_idx != self.last_avail_idx,
        None => false,
    }
}
```

### B. Re-check loop in block.rs `notify_queue()`

Wrap the existing drain logic in a loop:

```rust
fn notify_queue(&self, queue_idx: u16, queues: &mut [VirtQueue], physmap: &PhysMap) -> bool {
    let queue = match queues.get_mut(queue_idx as usize) {
        Some(q) => q,
        None => return false,
    };
    let completion = self.get_completion(queue);
    let mut requests: Vec<(u16, VirtioBlkReqHdr, Vec<ChainBuf>)> = Vec::new();

    loop {
        while let Some(head) = queue.pop_avail(physmap) {
            let chain = match queue.collect_chain(physmap, head) {
                Some(c) => c,
                None => continue,
            };
            // ... existing validation (chain len, header parse, bounds check) ...
            requests.push((head, header, chain));
        }
        queue.update_used_event(physmap);
        if !queue.has_new_avail(physmap) {
            break;
        }
        // Guest added entries between drain and used_event write; loop back
    }

    // ... existing processing (inline / worker dispatch) ...
}
```

### C. Same re-check in mod.rs default `notify_queue()` (affects VirtioRng)

```rust
fn notify_queue(&self, queue_idx: u16, queues: &mut [VirtQueue], physmap: &PhysMap) -> bool {
    let queue = match queues.get_mut(queue_idx as usize) {
        Some(q) => q,
        None => return false,
    };
    let mut did_work = false;
    loop {
        while let Some(head) = queue.pop_avail(physmap) {
            let written = self.process_queue(queue_idx, queue, head, physmap);
            queue.push_used(physmap, head, written);
            did_work = true;
        }
        queue.update_used_event(physmap);
        if !queue.has_new_avail(physmap) {
            break;
        }
    }
    did_work
}
```

### D. Uncomment EVENT_IDX in pci.rs line 298-300

```rust
bits::LEGACY_REG_DEVICE_FEATURES => {
    self.device.device_features()
        | bits::VIRTIO_F_RING_EVENT_IDX as u32
}
```

### E. Test at pci.rs:575 now correct as-is (expects EVENT_IDX bit)

## Correctness Argument

After `update_used_event()` writes `used_event = last_avail_idx`:
- Any entry with `avail_idx <= last_avail_idx` was already processed
- If `avail_idx > last_avail_idx` (new entries), the re-check catches them
- No window where entries are missed

This is the standard fix used by QEMU, Propolis, and Firecracker.

## Verification

- `cargo test -p vmm_devices` -- all tests pass (including `device_features_read`)
- 10 boot cycles with Linux guest -- no hangs
- `fio --rw=randread --iodepth=32` -- compare interrupt counts before/after (dtrace vmm::vioblk_notify)
- `fio --rw=randrw --iodepth=128 --numjobs=8 --runtime=300` stress test -- no errors or hangs
