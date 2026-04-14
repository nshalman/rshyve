## Vulnerability Analysis: `common.rs`

**Verdict: No directly exploitable vulnerabilities in this file.**

This is a clean, well-written set of I/O primitive types. Here's why:

### What's safe

- **No buffer overflow**: Fixed `[u8; 8]` buffers with `assert!` guards at construction (`len > 0 && len <= 8`). Assertions are `assert!`, not `debug_assert!`, so they fire in release builds too.
- **No uninitialized memory**: Buffers are zero-initialized (`[0u8; 8]`), so `buf()` never returns garbage.
- **No heap allocation**: Both types are `Copy` stack values — no use-after-free, double-free, or dangling pointers possible.
- **No interior mutability / race conditions**: No `Cell`, `RefCell`, atomics, or shared state.
- **Private fields**: `buf` and `len` are private, so `len` can't be mutated after construction by external code.

### One design-level concern (Low severity)

**Width mismatch on typed accessors** — The `write_u32()` / `read_u32()` etc. methods don't assert that `self.len` is large enough for the access width:

```rust
// ReadOp::new(1) creates a 1-byte op, but:
op.write_u64(secret);  // writes all 8 bytes into buffer
op.buf();              // returns only &buf[..1] — 7 bytes silently dropped

// WriteOp::from_buf(&[0xFF]) creates a 1-byte op, but:
op.read_u32();  // returns 0x000000FF — 3 zero bytes weren't guest-supplied
```

This isn't exploitable *from this file* because:
- The extra written bytes never escape (buf() truncates correctly)
- The extra read bytes are always zero (buffer is zero-initialized)

But it could cause **logic bugs in consuming device handlers** — e.g., a handler that calls `read_u32()` without checking `wo.len() >= 4` may misinterpret a 1-byte guest write as a 32-bit value. Whether that's exploitable depends on the specific device emulation code.

### Recommendation for the CTF

This file is the foundation layer — look at the **device handlers that consume these types** for the real bugs. Specifically, grep for `read_u32`/`read_u16` calls on `WriteOp` that don't first check `.len()` — those are the spots where a guest could send a short write and trick the handler into reading zero-padded values.
