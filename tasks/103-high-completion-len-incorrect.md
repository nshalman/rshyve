# BUG: Completion len always 1 (spec violation)

**Priority:** High
**File:** `crates/vmm-devices/src/virtio/block.rs` lines 290, 353

## Problem

`completion.complete(head, 1)` always passes `len=1`. VirtIO spec 2.7.8 says the `len`
field in the used ring entry should be the total bytes written to device-writable buffers:

- `T_IN` (read): sum of data buffer lengths + 1 (status byte)
- `T_OUT` (write): 1 (just status byte is writable)
- `T_FLUSH`: 1 (just status byte)
- Error cases: 1

Some BSD drivers check this for short-read detection. Most Linux drivers ignore it, but
spec compliance matters for a production VMM.

## Fix

Add a helper function:

```rust
fn used_len_for_request(rtype: u32, chain: &[ChainBuf], status: u8) -> u32 {
    if status == bits::VIRTIO_BLK_S_OK && rtype == bits::VIRTIO_BLK_T_IN {
        // Sum all writable buffer lengths (data + status byte)
        chain.iter()
            .filter_map(|b| match b {
                ChainBuf::Writable { len, .. } => Some(*len),
                _ => None,
            })
            .sum::<u32>()
    } else {
        1 // only status byte written
    }
}
```

Update inline path (line 290):
```rust
let used_len = used_len_for_request(header.rtype, chain, status);
completion.complete(head, used_len);
```

Update worker path (line 353) -- `BlkIoRequest` already has `rtype` and `chain`:
```rust
let used_len = used_len_for_request(req.rtype, &req.chain, status);
req.completion.complete(req.head, used_len);
```

## Verification

- `cargo test -p vmm_devices` passes
- `fio --rw=read --verify=crc32` produces correct data
- Boot FreeBSD guest (its virtio-blk driver checks used len)
