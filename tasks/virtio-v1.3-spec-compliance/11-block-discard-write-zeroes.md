# 11 — Implement DISCARD and WRITE_ZEROES commands

**Spec:** Section 5.2.6.2-3 — VIRTIO_BLK_T_DISCARD (11), VIRTIO_BLK_T_WRITE_ZEROES (13)
**Severity:** Medium (important for thin provisioning, SSD lifetime, guest performance)
**Effort:** ~80 lines

## Problem

The block device does not support DISCARD or WRITE_ZEROES. Guests cannot TRIM/UNMAP unused sectors, leading to:
- No thin provisioning support (disk images grow but never shrink)
- Reduced SSD lifetime (no TRIM forwarding)
- Slower guest filesystem operations (no hole punching)

## Spec details

### DISCARD request format
```
struct virtio_blk_discard_write_zeroes {
    le64 sector;        // Starting sector
    le32 num_sectors;   // Number of sectors
    le32 flags;         // Reserved (must be 0 for discard)
};
```
Multiple segments can be submitted in a single request (up to `max_discard_seg` segments).

### WRITE_ZEROES request format
Same structure. `flags` bit 0 = unmap (hint that the range may be deallocated).

### Feature bits
- `VIRTIO_BLK_F_DISCARD` (bit 13)
- `VIRTIO_BLK_F_WRITE_ZEROES` (bit 14)

### Config fields (when features offered)
- `max_discard_sectors` (u32) at offset 0x24
- `max_discard_seg` (u32) at offset 0x28
- `discard_sector_alignment` (u32) at offset 0x2C
- `max_write_zeroes_sectors` (u32) at offset 0x30
- `max_write_zeroes_seg` (u32) at offset 0x34
- `write_zeroes_may_unmap` (u8) at offset 0x38

## Files to modify

1. `crates/vmm-devices/src/virtio/bits.rs` — feature bits and request type constants
2. `crates/vmm-devices/src/virtio/block.rs` — request handling, config space, features

## Changes

### bits.rs

```rust
pub const VIRTIO_BLK_F_DISCARD: u64 = 1 << 13;
pub const VIRTIO_BLK_F_WRITE_ZEROES: u64 = 1 << 14;
pub const VIRTIO_BLK_T_DISCARD: u32 = 11;
pub const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;
```

### block.rs — feature advertisement

```rust
fn device_features(&self) -> u32 {
    let mut f = bits::VIRTIO_BLK_F_FLUSH as u32
        | bits::VIRTIO_BLK_F_DISCARD as u32
        | bits::VIRTIO_BLK_F_WRITE_ZEROES as u32;
    // ...
}
```

### block.rs — request handling

```rust
bits::VIRTIO_BLK_T_DISCARD => {
    // Parse discard segments from the data buffers
    for buf in &chain[1..chain.len() - 1] {
        if let ChainBuf::Readable { addr, len } = buf {
            let n = *len as usize;
            // Each segment is 16 bytes
            for i in (0..n).step_by(16) {
                if let Some(sub) = physmap.lookup(*addr + i as u64, 16) {
                    if let Ok(seg) = sub.read::<DiscardWriteZeroesSeg>() {
                        let offset = seg.sector * 512;
                        let len = seg.num_sectors as u64 * 512;
                        // Use fallocate(FALLOC_FL_PUNCH_HOLE) on Linux/illumos
                        let rc = unsafe {
                            libc::fallocate(
                                self.fd,
                                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                                offset as i64,
                                len as i64,
                            )
                        };
                        if rc != 0 {
                            return (bits::VIRTIO_BLK_S_IOERR, 1);
                        }
                    }
                }
            }
        }
    }
    (bits::VIRTIO_BLK_S_OK, 1)
}

bits::VIRTIO_BLK_T_WRITE_ZEROES => {
    // Similar to discard but uses fallocate(FALLOC_FL_ZERO_RANGE)
    // or explicit zero writes as fallback
    // ...
}
```

Note: On illumos, `fallocate` may not be available. Use `fcntl(F_FREESP)` for discard (hole punching) and `pwrite` of zeroes as fallback for WRITE_ZEROES.

### block.rs — config space

Extend config buffer to include discard/write_zeroes limits:

```rust
// max_discard_sectors at offset 0x24
config[0x24..0x28].copy_from_slice(&(u32::MAX).to_le_bytes());
// max_discard_seg at offset 0x28
config[0x28..0x2C].copy_from_slice(&1u32.to_le_bytes());
// max_write_zeroes_sectors at offset 0x30
config[0x30..0x34].copy_from_slice(&(u32::MAX).to_le_bytes());
// max_write_zeroes_seg at offset 0x34
config[0x34..0x38].copy_from_slice(&1u32.to_le_bytes());
// write_zeroes_may_unmap at offset 0x38
config[0x38] = 1;
```

Increase `BLK_CONFIG_SIZE_EXT` to accommodate the new fields.

## Platform considerations

- **illumos:** `fcntl(fd, F_FREESP, &flock)` for hole punching. For zero-fill, may need `pwrite` of zeroes.
- **Linux:** `fallocate(FALLOC_FL_PUNCH_HOLE)` for discard, `fallocate(FALLOC_FL_ZERO_RANGE)` for write_zeroes.
- **ZFS:** Discard may be a no-op on ZFS (COW filesystem), but it still frees blocks.

## Test

```rust
#[test]
fn block_discard_punches_hole() {
    // Create a temp file, write data, submit DISCARD request
    // Verify the file has a hole (lseek SEEK_HOLE)
}

#[test]
fn block_write_zeroes_clears_data() {
    // Create a temp file with non-zero data
    // Submit WRITE_ZEROES request
    // Read back — should be zeroes
}
```

Run: `cargo test -p vmm-devices -- block`
