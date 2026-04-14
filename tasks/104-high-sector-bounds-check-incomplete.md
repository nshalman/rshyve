# BUG: Incomplete sector bounds check

**Priority:** High
**File:** `crates/vmm-devices/src/virtio/block.rs` lines 261-268

## Problem

The bounds check is `header.sector >= self.capacity` which catches reads/writes starting
past EOF but NOT reads/writes that start within bounds and extend past the end.

Example: capacity=100 sectors, read of 8 sectors at sector 97 passes the check.
The subsequent `pread` returns a short read, caught by `rc != n` → IOERR. Not silent
corruption, but should be rejected early with a clean error.

## Fix

Replace the bounds check block at lines 261-268:

```rust
// Bounds check: both start AND end must be within capacity
if header.rtype == bits::VIRTIO_BLK_T_IN
    || header.rtype == bits::VIRTIO_BLK_T_OUT
{
    let data_bytes: u64 = chain[1..chain.len() - 1]
        .iter()
        .map(|b| match b {
            ChainBuf::Readable { len, .. }
            | ChainBuf::Writable { len, .. } => u64::from(*len),
        })
        .sum();
    let data_sectors =
        (data_bytes + bits::SECTOR_SIZE - 1) / bits::SECTOR_SIZE;

    if header.sector >= self.capacity
        || data_sectors > self.capacity - header.sector
    {
        write_status(physmap, &chain, bits::VIRTIO_BLK_S_IOERR);
        completion.complete(head, 1);
        continue;
    }
}
```

The second condition is overflow-safe: if `header.sector < self.capacity` (guaranteed by
short-circuit OR), then `self.capacity - header.sector` does not underflow.

## Verification

- `cargo test -p vmm_devices` passes
- Unit test: VirtioBlock backed by small file (8 sectors), attempt read of 2 sectors at sector 7 → IOERR
- Boot Linux guest, `dd if=/dev/vda bs=512 count=1 skip=$((last_sector))` succeeds
