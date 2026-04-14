# 02 — Virtio-blk sector offset integer overflow

**Priority:** CRITICAL
**File:** `crates/vmm-devices/src/virtio/block.rs:373`
**Category:** Security — out-of-bounds host file access

## Problem

```rust
let mut offset = (sector * bits::SECTOR_SIZE) as i64;
```

`sector * 512` overflows a `u64` when `sector >= 2^55`. The bounds check at line 261-268 compares `sector >= self.capacity` which should catch this for reasonably sized disks, but:

1. If `self.capacity` was computed from a sparse file with `metadata.len()` close to `u64::MAX`, the capacity itself could be huge.
2. The overflow wraps to a small value, and the cast to `i64` can produce a negative offset. `pread` with negative offset is platform-dependent behavior.

The same pattern appears in `do_pwrite` at line 396.

## Fix

Use `checked_mul` for the sector-to-byte conversion in both `do_pread` and `do_pwrite`:

```rust
let mut offset = match sector.checked_mul(bits::SECTOR_SIZE) {
    Some(o) => o as i64,
    None => return bits::VIRTIO_BLK_S_IOERR,
};
// Also verify the i64 cast doesn't flip sign
if offset < 0 {
    return bits::VIRTIO_BLK_S_IOERR;
}
```

Also verify `self.capacity` computation at line 98 cannot overflow:

```rust
let capacity = metadata.len() / sector_size;
```

If `metadata.len()` returns a nonsensical value for a sparse file, capacity could be huge. Add a sanity clamp or validate that `capacity * sector_size` doesn't overflow.

## Testing

- Test with `sector = u64::MAX / 512` (boundary, just barely fits)
- Test with `sector = u64::MAX / 512 + 1` (first overflow)
- Test with `sector = u64::MAX`
