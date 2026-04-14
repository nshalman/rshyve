# Vulnerability Report: `bhyve-api/sys/src/structs.rs`

## Critical Finding: Off-by-One in `validate_name` — Missing Null Terminator

**Type:** CWE-170 Improper Null Termination / Off-by-One  
**Severity:** High  
**Location:** `validate_name()` function (~line 300)

### Root Cause

```rust
pub const VM_MAX_NAMELEN: usize = 128;

fn validate_name(value: &[u8]) -> Result<[u8; VM_MAX_NAMELEN]> {
    let mut buf = [0u8; VM_MAX_NAMELEN];

    if value.len() > buf.len() {   // BUG: should be >=
        return Err(/* ... */);
    }

    buf[..(value.len())].copy_from_slice(value);
    Ok(buf)
}
```

The check `value.len() > buf.len()` allows a 128-byte input to pass (128 > 128 is false). The buffer is initialized to zeros, but `copy_from_slice` then **overwrites all 128 bytes**. If the input contains no null bytes, the resulting buffer has **no null terminator**.

This buffer is embedded in `vm_create_req` and `vm_destroy_req`, which are `#[repr(C)]` structs passed to the kernel via ioctl.

### Affected Structs

```rust
pub struct vm_create_req {
    pub name: [u8; VM_MAX_NAMELEN],  // ← 128 bytes, all non-null possible
    pub flags: u64,                   // ← kernel reads into here as part of "name"
}

pub struct vm_destroy_req {
    pub name: [u8; VM_MAX_NAMELEN],  // ← same problem
}
```

### Exploitation Strategy

The kernel treats `name` as a C string. Without a null terminator, `strlen()`/`strcmp()` in kernel space reads past the 128-byte name field:

1. **Information leak via `vm_create_req`**: Craft a 128-byte name with no nulls. The kernel sees the `flags: u64` field as part of the name. If the kernel echoes the name back (error messages, `/dev` entries, procfs), it leaks the flags value — or worse, adjacent kernel stack/heap data if the struct is copied into a kernel buffer and then read back.

2. **Name collision / access control bypass**: Two VMs with the same 128-byte name prefix but different `flags` values would appear identical to `strlen`-based lookups but different to `memcmp`-based ones, enabling confused-deputy attacks on VM management.

3. **Destroy wrong VM**: Send a `vm_destroy_req` with a 128-byte name. The kernel's name comparison reads past the buffer, potentially matching an unintended VM if the bytes that follow happen to align.

### Proof of Concept

```rust
use bhyve_api::vm_create_req;

// 128 bytes, no null terminator
let evil_name = [0x41u8; 128]; // "AAA...A" filling entire buffer
let req = vm_create_req::new(&evil_name).unwrap(); // passes validation!

// req.name has NO null terminator
// kernel strlen(req.name) reads into req.flags and beyond
assert!(req.name.iter().all(|&b| b != 0)); // confirmed: no null byte

// Now the kernel sees name as "AAA...A" + whatever flags bytes decode to
// If flags = 0, the first \0 byte in flags acts as terminator at offset 128
// But if flags = VCF_RESERVOIR_MEM (1) or VCF_TRACK_DIRTY (2):
let mut req2 = req;
req2.flags = 0x4141414141414141; // flags bytes are also non-null
// kernel now reads name as 136+ bytes — classic buffer over-read
```

### Fix

```rust
fn validate_name(value: &[u8]) -> Result<[u8; VM_MAX_NAMELEN]> {
    let mut buf = [0u8; VM_MAX_NAMELEN];
    if value.len() >= buf.len() {  // >= not >  — reserve 1 byte for null
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "name length exceeds VM_MAX_NAMELEN",
        ));
    }
    buf[..value.len()].copy_from_slice(value);
    Ok(buf)
}
```

---

## Secondary: `vm_data_xfer.vdx_len` Unchecked Against `VM_DATA_XFER_LIMIT`

**Severity:** Medium

The constant `VM_DATA_XFER_LIMIT = 8192` is defined but **never enforced** in the Rust struct. The `Default` impl sets `vdx_len: 0`, but nothing prevents a caller from setting `vdx_len` to an arbitrary `u32` value while `vdx_data` points to a smaller allocation. The kernel side may trust `vdx_len` and perform an out-of-bounds read/write on the userspace buffer.

---

## Tertiary: Raw Pointer Structs Lack Size Invariants

**Severity:** Low (design concern)

Multiple structs pair a `*mut c_void` with a length field but provide no constructor or validation:
- `vm_fpu_state { buf, len }` 
- `vmm_dirty_tracker { vdt_pfns, vdt_len }`
- `vm_npt_operation { vno_bitmap, vno_len }`

A caller can construct these with mismatched pointer/length, leading to kernel read/write past the allocation boundary. These are standard FFI hazards but could be hardened with safe constructors.

---

**Bottom line for the CTF:** The `validate_name` off-by-one is your flag. A 128-byte name with no nulls passes validation, producing a non-null-terminated C string that causes the kernel to read past the name buffer into adjacent struct fields.
