# 03 — SubMapping::read/write missing alignment check — UB

**Priority:** CRITICAL
**File:** `crates/vmm-core/src/mem.rs:182-226`
**Category:** Security — undefined behavior

## Problem

`SubMapping::read<T>()` (line 182) and `write<T>()` (line 207) claim "bounds and alignment checked above" in their safety comments, but alignment is never checked. Only bounds are verified (`self.len < size_of::<T>()`).

The cast `self.ptr.as_ptr() as *const T` with `read_volatile` requires the pointer to be aligned to `align_of::<T>()`. Guest memory addresses are guest-controlled — a malicious guest can set a virtqueue descriptor table base address that is not 16-byte aligned, causing `read::<VirtqDesc>()` (which contains `u64`, requiring 8-byte alignment) to create an unaligned pointer.

On x86 unaligned access works at the hardware level, but it is still UB per the Rust reference. The compiler is free to optimize based on the alignment assumption (e.g., auto-vectorization using aligned SSE loads).

## Fix

Option A — Add alignment check (returns error on misalignment):

```rust
pub fn read<T: Copy>(&self) -> Result<T> {
    // ... existing prot check ...
    if self.len < std::mem::size_of::<T>() {
        return Err(Error::new(ErrorKind::InvalidInput, "mapping too small"));
    }
    if (self.ptr.as_ptr() as usize) % std::mem::align_of::<T>() != 0 {
        return Err(Error::new(ErrorKind::InvalidInput, "mapping not aligned"));
    }
    // Safety: bounds and alignment checked above.
    let val = unsafe { std::ptr::read_volatile(self.ptr.as_ptr() as *const T) };
    Ok(val)
}
```

Option B — Use `read_unaligned` (always works, no UB, slight perf cost):

```rust
let val = unsafe { std::ptr::read_unaligned(self.ptr.as_ptr() as *const T) };
```

Note: `read_unaligned` is NOT volatile. There is no `read_unaligned_volatile` in std. If volatile semantics are needed, either:
- Use option A (reject misaligned), or
- Copy byte-by-byte with volatile reads, or
- Use inline asm with a volatile unaligned load.

Option A is recommended since virtio spec requires aligned descriptor tables.

Apply the same fix to `write<T>`.

## Testing

- Test `read::<u64>()` from a 1-byte-offset SubMapping — must return error (option A) or succeed (option B)
- Test `read::<VirtqDesc>()` from an aligned address — must succeed
