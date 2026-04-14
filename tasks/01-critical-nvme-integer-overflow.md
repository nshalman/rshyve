# 01 — NVMe I/O bounds check integer overflow

**Priority:** CRITICAL
**File:** `crates/vmm-devices/src/nvme/mod.rs:1457`
**Category:** Security — guest-to-host escape vector

## Problem

The NVMe I/O bounds check uses unchecked addition:

```rust
if req.slba + u64::from(req.nlb) > total_blocks {
    return SC_INVALID_FIELD | SC_DNR;
}
```

A malicious guest can set `slba = u64::MAX` and `nlb = 1`. The addition wraps to `0`, which passes the bounds check. The subsequent `byte_offset = req.slba * u64::from(block_size)` also overflows. The wrapped `byte_offset` is passed directly to `preadv`/`pwritev` as `libc::off_t`, allowing the guest to read/write at arbitrary offsets in the host backing file.

The `byte_count` calculation on line 1453 (`req.nlb as usize * block_size as usize`) also overflows for large `nlb` values on 32-bit or with extreme block sizes.

## Fix

Replace the unchecked arithmetic with `checked_add` and `checked_mul`:

```rust
// Bounds check — overflow-safe
let end_block = match req.slba.checked_add(u64::from(req.nlb)) {
    Some(end) => end,
    None => return SC_INVALID_FIELD | SC_DNR,
};
if end_block > total_blocks {
    return SC_INVALID_FIELD | SC_DNR;
}

let byte_offset = match req.slba.checked_mul(u64::from(block_size)) {
    Some(off) => off,
    None => return SC_INVALID_FIELD | SC_DNR,
};

let byte_count = match (req.nlb as u64).checked_mul(u64::from(block_size)) {
    Some(n) => n as usize,
    None => return SC_INVALID_FIELD | SC_DNR,
};
```

## Testing

Add unit tests with boundary values:
- `slba = u64::MAX, nlb = 1` — must reject
- `slba = total_blocks - 1, nlb = 1` — must accept
- `slba = total_blocks, nlb = 0` — must accept (zero-length read)
- `slba = 0, nlb = u16::MAX` with small `total_blocks` — must reject
