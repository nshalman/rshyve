# 19 — Volatile semantics inconsistency in guest memory access

**Priority:** MEDIUM
**File:** `crates/vmm-core/src/mem.rs:228-279`
**Category:** Correctness — potential compiler optimization of guest memory ops

## Problem

`read<T>` (line 198) uses `read_volatile` and `write<T>` (line 223) uses `write_volatile`, but `read_bytes` (line 229) and `write_bytes` (line 256) use `copy_nonoverlapping` which is NOT volatile.

The file's documentation states all operations must use volatile because "guest vCPUs may read or write concurrently." The compiler is free to reorder, merge, or eliminate non-volatile memory operations.

On x86 this is unlikely to cause issues in practice (strong memory model, and `copy_nonoverlapping` of non-trivial size won't be eliminated), but it is a semantic violation of the documented safety model.

## Fix

Option A — Volatile byte copy (correct, slight perf cost):

```rust
pub fn read_bytes(&self, buf: &mut [u8]) -> Result<()> {
    // ... existing checks ...
    let src = self.ptr.as_ptr();
    for i in 0..buf.len() {
        buf[i] = unsafe { std::ptr::read_volatile(src.add(i)) };
    }
    Ok(())
}

pub fn write_bytes(&self, buf: &[u8]) -> Result<()> {
    // ... existing checks ...
    let dst = self.ptr.as_ptr();
    for i in 0..buf.len() {
        unsafe { std::ptr::write_volatile(dst.add(i), buf[i]) };
    }
    Ok(())
}
```

Option B — Document the exception (pragmatic):

Add a comment explaining why `copy_nonoverlapping` is acceptable for bulk byte operations:

```rust
// Note: We use copy_nonoverlapping (non-volatile) for bulk byte copies.
// While read/write<T> use volatile for single-value access, bulk copies
// are always large enough that the compiler won't optimize them away.
// The x86 memory model ensures these writes are visible to other cores.
```

Option A is technically correct. Option B is pragmatically acceptable with documentation.
