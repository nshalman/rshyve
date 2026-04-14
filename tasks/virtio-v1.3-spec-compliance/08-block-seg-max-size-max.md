# 08 — Expose seg_max and size_max in block config space

**Spec:** Section 5.2.4 — Block device config includes `size_max` (max segment size), `seg_max` (max segments per request), `geometry`, `topology`, etc. when corresponding feature bits are set.
**Severity:** Medium (guest can't size I/O correctly without these)
**Effort:** ~30 lines

## Problem

The block device config space (`block.rs:199-213`) only exposes:
- `capacity` (u64) at offset 0
- `blk_size` (u32) at offset 0x18

It does NOT expose `size_max` or `seg_max`, which tell the guest the maximum single-segment size and maximum number of segments per request. Without these, the guest uses its own defaults which may exceed what the device can handle.

## Spec layout (Section 5.2.4)

```
struct virtio_blk_config {
    le64 capacity;        // offset 0x00
    le32 size_max;        // offset 0x08 (if VIRTIO_BLK_F_SIZE_MAX)
    le32 seg_max;         // offset 0x0C (if VIRTIO_BLK_F_SEG_MAX)
    struct virtio_blk_geometry {  // offset 0x10 (if VIRTIO_BLK_F_GEOMETRY)
        le16 cylinders;
        u8 heads;
        u8 sectors;
    };
    le32 blk_size;        // offset 0x14 (if VIRTIO_BLK_F_BLK_SIZE)
    // ... more fields ...
};
```

Note: `blk_size` is at offset **0x14**, not 0x18 as currently implemented.

## Files to modify

1. `crates/vmm-devices/src/virtio/bits.rs` — add feature bits and constants
2. `crates/vmm-devices/src/virtio/block.rs` — update config space and features

## Changes

### bits.rs — add feature bits

```rust
/// Maximum size of any single segment.
pub const VIRTIO_BLK_F_SIZE_MAX: u64 = 1 << 1;
/// Maximum number of segments in a request.
pub const VIRTIO_BLK_F_SEG_MAX: u64 = 1 << 2;
```

### block.rs — update device_features

```rust
fn device_features(&self) -> u32 {
    let mut f = bits::VIRTIO_BLK_F_FLUSH as u32
        | bits::VIRTIO_BLK_F_SIZE_MAX as u32
        | bits::VIRTIO_BLK_F_SEG_MAX as u32;
    if self.read_only { f |= bits::VIRTIO_BLK_F_RO as u32; }
    if self.sector_size != 512 { f |= bits::VIRTIO_BLK_F_BLK_SIZE as u32; }
    f
}
```

### block.rs — update cfg_read

Update the config buffer to include size_max and seg_max at their spec-defined offsets:

```rust
fn cfg_read(&self, offset: u16, len: u8) -> u32 {
    let mut config = [0u8; 28];

    // capacity (offset 0x00, 8 bytes)
    config[0x00..0x08].copy_from_slice(&self.capacity.to_le_bytes());
    // size_max (offset 0x08, 4 bytes) — max 1MB per segment
    config[0x08..0x0C].copy_from_slice(&(1024u32 * 1024).to_le_bytes());
    // seg_max (offset 0x0C, 4 bytes) — max 126 segments per request
    // (virtio descriptor chain max minus header and status)
    config[0x0C..0x10].copy_from_slice(&126u32.to_le_bytes());
    // blk_size (offset 0x14, 4 bytes) — note: offset 0x14, NOT 0x18
    if self.sector_size != 512 {
        config[0x14..0x18].copy_from_slice(&self.sector_size.to_le_bytes());
    }

    // ... existing byte extraction ...
}
```

**Important:** Fix the `blk_size` offset from 0x18 to 0x14 per the spec. The current offset 0x18 is wrong — it would be the offset if geometry were populated, but geometry is not offered.

Wait — re-check this. The config struct has geometry at 0x10 (4 bytes) and blk_size at 0x14. If `VIRTIO_BLK_F_GEOMETRY` is NOT offered, the fields are still at their spec-defined offsets. The spec says "corresponding fields are not available" but the offset doesn't change — the config space is a fixed struct, not a packed variable-length buffer.

So `blk_size` at 0x14 is correct per the spec. The current code using 0x18 is wrong. Fix it to 0x14.

## Test

```rust
#[test]
fn block_config_size_max_and_seg_max() {
    let blk = VirtioBlock::new(/* ... */);
    // Read config at offset 0x08 (size_max)
    let size_max = blk.cfg_read(0x08, 4);
    assert_eq!(size_max, 1024 * 1024);
    // Read config at offset 0x0C (seg_max)
    let seg_max = blk.cfg_read(0x0C, 4);
    assert_eq!(seg_max, 126);
}
```

Run: `cargo test -p vmm-devices -- block`
