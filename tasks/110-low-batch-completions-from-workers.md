# PERF: Batch completions from worker threads

**Priority:** Low
**File:** `crates/vmm-devices/src/virtio/block.rs`

## Problem

Each worker calls `completion.complete()` per request, which:
1. Acquires the `write_idx` Mutex
2. Writes one used ring entry
3. Releases the Mutex
4. Potentially raises an interrupt

At high iodepth, this means N Mutex acquisitions and up to N interrupts for N requests.
`VirtioCompletion::complete_batch()` exists (queue.rs:560) but is never used.

## Fix

Workers accumulate completed requests locally, then batch-complete:

```rust
fn blk_io_worker_dedicated(
    rx: mpsc::Receiver<BlkIoRequest>,
    fd: i32,
    read_only: bool,
    physmap: Arc<PhysMap>,
) {
    let mut batch: Vec<(u16, u32)> = Vec::with_capacity(16);

    loop {
        // Block on first request
        let req = match rx.recv() {
            Ok(r) => r,
            Err(_) => return,
        };

        let comp = Arc::clone(&req.completion);
        let status = process_one(&req, fd, read_only, &physmap);
        write_status(&physmap, &req.chain, status);
        batch.push((req.head, used_len_for_request(req.rtype, &req.chain, status)));

        // Drain any additional ready requests (non-blocking)
        while let Ok(req) = rx.try_recv() {
            let status = process_one(&req, fd, read_only, &physmap);
            write_status(&physmap, &req.chain, status);
            batch.push((req.head, used_len_for_request(req.rtype, &req.chain, status)));
        }

        // One Mutex acquisition, one interrupt decision for entire batch
        comp.complete_batch(&batch);
        batch.clear();
    }
}
```

Best combined with task 109 (per-worker channels) since the worker function is being
restructured anyway. With per-worker channels, `try_recv()` is lock-free.

## Verification

- `cargo test -p vmm_devices` passes
- dtrace interrupt count under `fio --iodepth=32` -- fewer interrupts than before
- `fio --rw=randrw --verify=crc32` correctness check
