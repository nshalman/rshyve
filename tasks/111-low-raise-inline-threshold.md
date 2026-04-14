# PERF: Raise inline processing threshold

**Priority:** Low
**File:** `crates/vmm-devices/src/virtio/block.rs` line 284

## Problem

The inline fast path only triggers for exactly 1 request (`requests.len() == 1`).
For 2-3 requests, the overhead of dispatching to workers (channel send, context switch,
Mutex contention) may exceed the cost of doing 2-3 sequential pread/pwrite calls inline
on the vCPU thread.

## Fix

Change the threshold:

```rust
const INLINE_THRESHOLD: usize = 3;

if requests.len() <= INLINE_THRESHOLD {
    for (head, header, ref chain) in &requests {
        probes::vioblk_inline!(|| (*head, header.rtype, header.sector));
        let status = self.process_inline(header, chain, physmap);
        write_status(physmap, chain, status);
        probes::vioblk_complete!(|| (*head, status));
        let used_len = used_len_for_request(header.rtype, chain, status);
        completion.complete(*head, used_len);
    }
    return false;
}
```

The optimal threshold depends on the disk backend latency. For ZFS zvols on illumos,
pread/pwrite typically completes in 5-50us, while channel dispatch + worker wakeup is
~10-20us overhead. Start with 3 and tune via benchmarking.

## Verification

- `cargo test -p vmm_devices` passes
- Benchmark `fio --iodepth=2` and `--iodepth=3` before/after -- expect latency improvement
- dtrace `vmm::vioblk_inline` vs `vmm::vioblk_dispatch` probes to confirm threshold
- No regression at `--iodepth=32` (still dispatches to workers)
