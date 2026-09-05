# virtio-blk bounce cost

The virtio-blk data path no longer runs `preadv`/`pwritev` straight into
and out of guest memory. Every request now bounces through a per-worker
host buffer, and the inline small-read fast path is gone, so every
backing-store read dispatches to a worker.

Why the copy is paid: a backing-store operation must own only host
memory. That is what lets a device reset be synchronous, which legacy
virtio needs, and bounded, because the reset then waits for memory
copies and ring writes and never for the disk.

This diverges from Propolis, which holds a memory accessor across the
syscall and reads and writes guest memory directly
(`lib/propolis/src/block/file.rs:96` and `:109`).

## What was measured

illumos, x86_64, one host. The memcpy and the worker hand-off in
isolation, per operation, median of 5. The chain is one segment. This is
not an end-to-end measurement.

| size | read before | read after | change | write before | write after | change |
|---|---|---|---|---|---|---|
| 4 KiB | 922 ns | 1027 ns | +11.4% | 987 ns | 1046 ns | +6.0% |
| 16 KiB | 2005 ns | 2616 ns | +30.5% | 2244 ns | 2893 ns | +29.0% |
| 64 KiB | 6037 ns | 8388 ns | +38.9% | 6972 ns | 9375 ns | +34.5% |
| 128 KiB | 11441 ns | 16095 ns | +40.7% | 13265 ns | 18040 ns | +36.0% |
| 256 KiB | 58596 ns | 31771 ns | -45.8% | 25967 ns | 36017 ns | +38.7% |

The 256 KiB read is faster after the change, and that reproduces:
illumos `preadv` on one large iovec is slower than `pread`.

One segment is the cheapest case for the old path. A real chain carries
up to `seg_max` segments, which this device advertises as 126, so these
numbers understate what the old path cost at the large sizes.

Worker hand-off, which the removed inline path used to avoid: 1127 to
1201 ns one way.

Small-read latency, 4 KiB at iodepth 1, device-side service time: about
0.92 us inline before, about 2.2 us after. That is about +1.3 us, or
2.4x, and the thread hand-off dominates it, not the copy.

## What was not measured

No guest was booted and fio was not run. Nothing here reports guest
IOPS, VM exit cost, interrupt injection, vring overhead, or a zvol
backing store. Numbers are comparable only against a later run on the
same host.
