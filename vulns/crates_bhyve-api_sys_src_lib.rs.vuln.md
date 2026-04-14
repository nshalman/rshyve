Now I have the full picture. Here's the vulnerability report:

---

# Vulnerability Report: `crates/bhyve-api/sys/src/` (and consumers)

## Finding 1: Off-by-One in `validate_name` — Missing Null Terminator

**Severity:** Medium  
**Type:** Off-by-one / buffer over-read (kernel-side)  
**File:** `crates/bhyve-api/sys/src/structs.rs:505-517`

### Root Cause

```rust
fn validate_name(value: &[u8]) -> Result<[u8; VM_MAX_NAMELEN]> {
    let mut buf = [0u8; VM_MAX_NAMELEN];  // 128 bytes, zero-initialized

    if value.len() > buf.len() {   // BUG: allows value.len() == 128
        return Err(/* ... */);
    }

    buf[..(value.len())].copy_from_slice(value);
    Ok(buf)
}
```

The check is `>` when it should be `>=`. When `value` is exactly 128 bytes of non-null data, the entire buffer is filled with **no null terminator**. This buffer becomes `vm_create_req.name` or `vm_destroy_req.name`, passed to the kernel via ioctl.

### Exploitation Strategy

The kernel receives a `[u8; 128]` with no NUL byte. If the kernel's VMM code uses `strlen()` or any unbounded string operation on this name, it reads past the struct boundary into adjacent kernel memory. This can:

1. **Create a VM with a name leaking kernel stack/heap bytes** — the name stored internally will include whatever bytes follow the struct in kernel memory until a `\0` is hit.
2. **Name collision attacks** — two different 128-byte names could resolve to the same internal name if the kernel truncates, allowing one user to operate on another user's VM via `vm_destroy_req`.
3. **Path traversal via `/dev/vmm/<name>`** — if the over-read bytes happen to include path separators, the resulting device node path could escape the expected directory.

### Proof of Concept Sketch

```rust
use bhyve_api::sys::*;

// Fill exactly 128 bytes — no room for null terminator
let name = [b'A'; VM_MAX_NAMELEN];  // 128 bytes, no NUL
let req = vm_create_req::new(&name).unwrap();  // succeeds! check is >
// req.name is [0x41; 128] — no null terminator
// Kernel ioctl(VMM_CREATE_VM, &req) reads name past buffer
```

### Fix

```rust
if value.len() >= buf.len() {  // >= instead of >
```

---

## Finding 2: Missing Bitmap Size Validation in `set_dirty_pages`

**Severity:** High  
**Type:** Buffer over-read → kernel reads past Rust slice boundary  
**File:** `crates/vmm-core/src/hdl.rs:411-441`

### Root Cause

Compare the **safe** function `track_dirty_pages` (line 377) which validates:

```rust
let pages = (len + 4095) / 4096;
let required_bytes = (pages + 7) / 8;
if bitmap.len() < required_bytes {
    return Err(/* ... */);
}
```

With the **vulnerable** `set_dirty_pages` (line 411) which does **no validation**:

```rust
pub fn set_dirty_pages(
    &self,
    gpa: u64,
    len: u64,        // attacker-controlled region length
    bitmap: &[u8],   // NO check that bitmap is large enough
) -> Result<()> {
    // ...
    let mut op = vm_npt_operation {
        vno_gpa: gpa,
        vno_len: len,                              // can be huge
        vno_bitmap: bitmap.as_ptr() as *mut u8,    // can be tiny
        vno_operation: VNO_OP_SET_DIRTY | VNO_FLAG_BITMAP_IN,
    };
    unsafe { self.inner.ioctl(ioctls::VM_NPT_OPERATION, &mut op) }?;
```

The kernel reads `ceil(len / 4096 / 8)` bytes from the bitmap pointer. If `len` describes a 1 GiB region but `bitmap` is 1 byte, the kernel reads ~32 KiB past the Rust slice boundary.

### Exploitation Strategy

1. Allocate a small `bitmap: [u8; 1]` at a known heap position.
2. Call `set_dirty_pages(0, 1 << 30, &bitmap)` — claiming 1 GiB of guest pages.
3. The kernel `copyin()`s ~32 KiB from the process's address space starting at `bitmap.as_ptr()`. Whatever heap data follows the bitmap (secrets, pointers, other VM state) is interpreted as dirty-page bits and **applied to the NPT**.
4. By controlling the heap layout, an attacker can selectively mark specific guest pages as dirty, corrupting migration state or causing the hypervisor to re-send stale pages during live migration — a **VM escape primitive** if combined with a guest-side write.

### Fix

Add the same validation as `track_dirty_pages`:

```rust
let pages = ((len + 4095) / 4096) as usize;
let required_bytes = (pages + 7) / 8;
if bitmap.len() < required_bytes {
    return Err(/* ... */);
}
```

---

## Finding 3: `VM_DATA_XFER_LIMIT` Defined But Never Enforced

**Severity:** Low  
**Type:** Missing bounds check  
**File:** `crates/bhyve-api/sys/src/structs.rs:377` (constant), `crates/bhyve-api/src/lib.rs:352-382` (usage)

### Root Cause

```rust
pub const VM_DATA_XFER_LIMIT: u32 = 8192;  // defined in structs.rs
```

But `read_all()` trusts the kernel's `vdx_result_len` to determine allocation size and never checks against this limit. If the kernel returns a `result_len > 8192`, the code allocates and fills a larger-than-expected buffer. In a threat model where the kernel can be compromised (e.g., via another bug), this becomes a userspace heap overflow.

---

## Finding 4: Unsafe `set_len` on Kernel-Controlled Size

**Severity:** Low (requires compromised kernel)  
**Type:** Uninitialized memory exposure  
**File:** `crates/bhyve-api/src/lib.rs:370-380`

```rust
let mut data: Vec<T> = Vec::with_capacity(item_count as usize);
// ... ioctl fills some of data ...
unsafe {
    data.set_len((bytes_read / item_len) as usize);  // trusts kernel
}
```

If `bytes_read` is smaller than what was actually written, or the ioctl fails silently, uninitialized heap memory is exposed as valid `T` items.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|---|---|---|
| 1 | `validate_name` off-by-one (no null terminator) | **Medium** | Yes — kernel over-read, name collision |
| 2 | `set_dirty_pages` missing bitmap validation | **High** | Yes — kernel reads past heap allocation |
| 3 | `VM_DATA_XFER_LIMIT` unenforced | Low | Only with compromised kernel |
| 4 | `set_len` trusts kernel-reported size | Low | Only with compromised kernel |

**For CTF: Finding #2 is the most exploitable** — it's a concrete missing validation that the sibling function (`track_dirty_pages`) correctly implements, making the inconsistency obvious and the fix provable.
