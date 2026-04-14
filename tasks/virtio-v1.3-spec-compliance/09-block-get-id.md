# 09 — Add GET_ID command support

**Spec:** Section 5.2.6.1 — VIRTIO_BLK_T_GET_ID (type 8) returns a 20-byte NUL-padded device identification string.
**Severity:** Low-Medium (used by guests for `/dev/disk/by-id/`)
**Effort:** ~20 lines

## Problem

The block device handles IN (0), OUT (1), and FLUSH (4) but returns `VIRTIO_BLK_S_UNSUPP` for GET_ID (8). Some guests (Linux, Windows) use GET_ID to discover a stable device identifier.

## Files to modify

1. `crates/vmm-devices/src/virtio/bits.rs` — add constant
2. `crates/vmm-devices/src/virtio/block.rs` — handle in process_inline and worker path

## Changes

### bits.rs

```rust
/// Get device identification string (20 bytes, NUL-padded).
pub const VIRTIO_BLK_T_GET_ID: u32 = 8;
```

### block.rs — process_inline (line ~169)

Add a new match arm:

```rust
bits::VIRTIO_BLK_T_GET_ID => {
    // Write a 20-byte NUL-padded device ID string to the first
    // writable buffer in the chain.
    let id = self.device_id();
    if chain.len() < 2 {
        return (bits::VIRTIO_BLK_S_IOERR, 1);
    }
    if let ChainBuf::Writable { addr, len } = &chain[chain.len() - 2] {
        let write_len = (*len as usize).min(20);
        if let Some(sub) = physmap.lookup(*addr, write_len) {
            let _ = sub.write_bytes(&id[..write_len]);
        }
    }
    (bits::VIRTIO_BLK_S_OK, 21) // 20 bytes ID + 1 byte status
}
```

### block.rs — add device_id helper

```rust
/// Return a 20-byte device identification string.
///
/// Uses the backing file path truncated/padded to 20 bytes.
/// A future enhancement could use the VM UUID or a CLI-specified serial.
fn device_id(&self) -> [u8; 20] {
    let mut id = [0u8; 20];
    // Use a hash or truncation of the path as the ID
    let src = format!("vmmnew-blk-{}", self.fd);
    let bytes = src.as_bytes();
    let copy_len = bytes.len().min(20);
    id[..copy_len].copy_from_slice(&bytes[..copy_len]);
    id
}
```

A better approach would be to store a configurable serial number at construction time (e.g., from CLI `--block-serial` or derived from VM UUID). For now, a deterministic fd-based ID is sufficient.

### block.rs — worker path

The worker path (`io_worker` function) also needs to handle GET_ID. Since GET_ID doesn't do disk I/O, it's simplest to handle it inline only. In `notify_queue`, GET_ID requests will always take the inline path (iodepth=1 for a single GET_ID).

## Test

```rust
#[test]
fn block_get_id_returns_string() {
    // Create VirtioBlock
    // Submit a GET_ID request (type=8) with a 20-byte writable buffer + status byte
    // Verify the writable buffer contains a non-empty NUL-padded string
    // Verify status is VIRTIO_BLK_S_OK
}
```

Run: `cargo test -p vmm-devices -- block`
