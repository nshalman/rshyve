# PERF: Eliminate Vec allocation for iodepth=1

**Priority:** Medium
**File:** `crates/vmm-devices/src/virtio/block.rs` line 237

## Problem

`notify_queue()` allocates `Vec<(u16, VirtioBlkReqHdr, Vec<ChainBuf>)>` on every call.
For iodepth=1 (the overwhelmingly common case), this allocates a Vec, pushes 1 element,
processes it, and drops the Vec -- every single I/O.

## Fix

Restructure to avoid collecting when there's only one request. Pop the first request,
peek for a second. If no second exists, process inline immediately without Vec allocation.

```rust
fn notify_queue(&self, queue_idx: u16, queues: &mut [VirtQueue], physmap: &PhysMap) -> bool {
    let queue = match queues.get_mut(queue_idx as usize) {
        Some(q) => q,
        None => return false,
    };
    let completion = self.get_completion(queue);

    // Pop first available -- if none, nothing to do
    let first_head = match queue.pop_avail(physmap) {
        Some(h) => h,
        None => {
            queue.update_used_event(physmap);
            // re-check for event_idx race (task 106)
            if queue.has_new_avail(physmap) {
                // rare: recurse or loop
            }
            return false;
        }
    };

    // Validate first request
    let first_chain = match queue.collect_chain(physmap, first_head) { ... };
    let first_header = match parse_header(&first_chain, physmap) { ... };
    // ... bounds check ...

    // Try second pop -- if None, single request fast path (no Vec)
    let second_head = queue.pop_avail(physmap);

    if second_head.is_none() {
        queue.update_used_event(physmap);
        if !queue.has_new_avail(physmap) {
            // iodepth=1: inline, zero allocation
            probes::vioblk_inline!(|| (first_head, first_header.rtype, first_header.sector));
            let status = self.process_inline(&first_header, &first_chain, physmap);
            write_status(physmap, &first_chain, status);
            let used_len = used_len_for_request(first_header.rtype, &first_chain, status);
            completion.complete(first_head, used_len);
            return false;
        }
        // new entries arrived, fall through to batch path
    }

    // Batch path: collect into Vec (only when iodepth > 1)
    let mut requests = Vec::new();
    requests.push((first_head, first_header, first_chain));
    if let Some(h) = second_head {
        // validate and push second...
    }
    // continue draining...
}
```

This is best combined with task 106 (event-idx re-check loop) since both restructure
the `notify_queue()` drain loop.

## Verification

- `cargo test -p vmm_devices` passes
- `fio --iodepth=1` workload: no regression, verify no heap allocation per I/O via dtrace
- `fio --iodepth=32` workload: correctness maintained
