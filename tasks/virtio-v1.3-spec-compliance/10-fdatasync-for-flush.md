# 10 — Use fdatasync instead of fsync for FLUSH

**Spec:** Section 5.2.6.1 — "A FLUSH request flushes the volatile write cache of the disk."
**Severity:** Low (fsync is strictly more than required; this is a performance improvement)
**Effort:** 2 lines

## Problem

`block.rs:176`:
```rust
bits::VIRTIO_BLK_T_FLUSH => {
    if unsafe { libc::fsync(self.fd) } == 0 {
```

`fsync` flushes both data and file metadata (timestamps, size). The spec only requires flushing the "volatile write cache" — data only. `fdatasync` flushes data without metadata, which is sufficient and faster (avoids an extra metadata write + journal update).

The same pattern appears in the worker thread path (`block.rs:371`).

## Files to modify

`crates/vmm-devices/src/virtio/block.rs` — 2 locations

## Changes

### Line 176 (inline path)
```rust
// Before:
if unsafe { libc::fsync(self.fd) } == 0 {

// After:
if unsafe { libc::fdatasync(self.fd) } == 0 {
```

### Line ~371 (worker path)
Same change — find the `fsync` in the worker and replace with `fdatasync`.

Note: `fdatasync` is available on both Linux and illumos via `libc::fdatasync`.

## Test

No functional test change needed — fdatasync is a subset of fsync. Performance improvement can be measured with:
```bash
fio --ioengine=sync --fsync=1 --bs=4k --rw=write --size=1G
```

Compare fsync vs fdatasync throughput (expect 10-30% improvement for metadata-heavy workloads).

Run: `cargo test -p vmm-devices -- block` (verify existing tests pass)
