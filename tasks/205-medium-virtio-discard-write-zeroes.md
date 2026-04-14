# 205 - MEDIUM - Add virtio-blk discard and write-zeroes support

## Problem

vmmnew's VirtioBlock only handles `VIRTIO_BLK_T_IN` (read), `VIRTIO_BLK_T_OUT` (write),
and `VIRTIO_BLK_T_FLUSH`. Missing:

- `VIRTIO_BLK_T_DISCARD` (11) — guest tells host a region is no longer needed
- `VIRTIO_BLK_T_WRITE_ZEROES` (13) — guest requests a region be zeroed

These are important for:
- ZFS zvol backends: discard → `DKIOCFREE` ioctl → space reclamation
- Thin-provisioned images: discard → `fallocate(FALLOC_FL_PUNCH_HOLE)`
- Guest `fstrim` / `blkdiscard` commands

## Design

### Feature bits

```rust
// In virtio/bits.rs:
pub const VIRTIO_BLK_F_DISCARD: u64 = 1 << 13;
pub const VIRTIO_BLK_F_WRITE_ZEROES: u64 = 1 << 14;
```

### Request types

```rust
// In virtio/block.rs:
pub const VIRTIO_BLK_T_DISCARD: u32 = 11;
pub const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;
```

### Discard/WriteZeroes payload (VirtIO 1.2 sec 5.2.6.2)

The guest sends a `VirtioBlkDiscardWriteZeroes` struct after the header:

```rust
#[repr(C)]
struct VirtioBlkDiscardWriteZeroes {
    sector: u64,       // starting sector
    num_sectors: u32,  // number of sectors
    flags: u32,        // bit 0: unmap (for write-zeroes)
}
```

Multiple segments can be chained in one request.

### Config space additions

Add to device config (after existing fields):
- offset 0x1C: `max_discard_sectors` (u32)
- offset 0x20: `max_discard_seg` (u32) — max segments per discard request
- offset 0x24: `discard_sector_alignment` (u32)
- offset 0x28: `max_write_zeroes_sectors` (u32)
- offset 0x2C: `max_write_zeroes_seg` (u32)

### Implementation in VirtioBlock

```rust
fn process_discard(&self, chain: &mut Chain, physmap: &PhysMap) -> u8 {
    while chain.remain_read() >= 16 {
        let seg: VirtioBlkDiscardWriteZeroes = chain.read_obj(physmap)?;
        let offset = seg.sector * self.sector_size as u64;
        let len = seg.num_sectors as u64 * self.sector_size as u64;
        match self.backend.discard(offset, len) {
            Ok(()) => {},
            Err(_) => return VIRTIO_BLK_S_IOERR,
        }
    }
    VIRTIO_BLK_S_OK
}
```

## Dependencies

- Task 204 (block backend trait) — `BlockBackend::discard()` and `write_zeroes()`
- Task 203 (chain abstraction) — `Chain::read_obj()` for parsing segments

Can be implemented without those deps by adding the handling directly in block.rs,
but cleaner with them.

## Files to modify

- `crates/vmm-devices/src/virtio/bits.rs` — Add feature bits
- `crates/vmm-devices/src/virtio/block.rs` — Add request types, process functions, config space entries
- `crates/vmm-devices/src/block.rs` (if task 204 done) — Implement discard/write_zeroes on FileBackend

## FileBackend discard implementation

```rust
fn discard(&self, offset: u64, len: u64) -> io::Result<()> {
    #[cfg(target_os = "illumos")]
    {
        // Use DKIOCFREE ioctl for zvols, or fallocate for files
        use std::os::unix::io::AsRawFd;
        // fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, offset, len)
        // illumos: use freerange(3C) or ioctl
    }
    Ok(()) // No-op fallback is safe — discard is advisory
}
```

## Verification

1. Boot Linux guest with virtio-blk
2. `lsblk -D` — verify discard granularity and max bytes reported
3. `blkdiscard /dev/vda` — should succeed
4. `fstrim -v /` — should report trimmed bytes
5. DTrace: add `vioblk_discard(head, sector, num_sectors)` probe
