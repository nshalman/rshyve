# 11 — getrandom() return value silently discarded

**Priority:** HIGH
**File:** `crates/vmm-devices/src/virtio/rng.rs:94-114`
**Category:** Security — guest receives zero "entropy"

## Problem

### illumos path (line 94-104):

```rust
fn getrandom(buf: &mut [u8]) {
    unsafe {
        libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0);
        // return value not checked
    }
}
```

`getrandom(2)` returns `ssize_t` — the number of bytes written, or -1 on error. On short read or error, the buffer remains zeroed (from `vec![0u8; n]` on line 68). The guest receives deterministic zero bytes as "random" data.

### Non-illumos fallback (line 107-114):

```rust
fn getrandom(buf: &mut [u8]) {
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(buf);
    }
    // If open fails: buf stays zeroed, no error
    // If read_exact fails: buf partially filled, no error
}
```

Both paths can silently deliver zero or partial entropy. For a `virtio-rng` device, this is catastrophic — the guest uses this for key generation, TLS, ASLR.

## Fix

```rust
#[cfg(target_os = "illumos")]
fn getrandom(buf: &mut [u8]) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let rc = unsafe {
            libc::getrandom(
                buf[filled..].as_mut_ptr().cast(),
                buf.len() - filled,
                0,
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        filled += rc as usize;
    }
    Ok(())
}

#[cfg(not(target_os = "illumos"))]
fn getrandom(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)?;
    Ok(())
}
```

In the caller (`notify_queue`), propagate the error by writing `VIRTIO_BLK_S_IOERR` to the status byte if `getrandom` fails, rather than silently delivering zeroed data.

## Testing

- Mock a failing getrandom (via cfg test override) and verify the guest gets an error status, not zeroes
- Verify the loop handles short reads correctly
