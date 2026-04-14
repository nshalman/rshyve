# Security Analysis: `crates/viona-api/src/ffi.rs`

## Summary

This file is an **FFI type/constant definitions file** — the Rust equivalent of a C header. It defines ioctl command codes and `#[repr(C)]` structs for communicating with the `viona` kernel driver (virtio-net). **There are no directly exploitable vulnerabilities in this file itself**, but there are several security-relevant patterns worth documenting for a CTF.

---

## Findings

### 1. Raw Pointers with User-Controlled Sizes (Medium)

**Affected structs:** `vioc_set_params` (lines ~128-133), `vioc_get_params` (lines ~122-125)

```rust
pub struct vioc_set_params {
    pub vsp_param: *mut c_void,
    pub vsp_param_sz: size_t,
    pub vsp_error: *mut c_void,
    pub vsp_error_sz: size_t,
}
```

The pointer and size are independently user-controlled. If the code that passes this to `ioctl()` doesn't enforce that `vsp_param_sz <= actual_allocation`, the kernel will `copyin`/`copyout` past the buffer boundary. The `#[derive(Default)]` gives null pointers and zero sizes, so partial initialization (setting param but forgetting error) could pass a null `vsp_error` to the kernel.

**This is the classic ioctl buffer overflow pattern** — the actual exploitability depends on the calling code and the kernel driver's validation. Look at how `VNA_IOC_SET_PARAMS` / `VNA_IOC_GET_PARAMS` are called.

### 2. User-Controlled MMIO Address (Medium)

**Affected struct:** `vioc_notify_mmio` (line ~108)

```rust
pub struct vioc_notify_mmio {
    pub vim_address: u64,
    pub vim_size: u32,
}
```

This lets the caller tell the kernel where MMIO notifications should be mapped. If the kernel doesn't validate that `vim_address` falls within the VM's legitimate address space, an attacker could point this at arbitrary physical or kernel memory. Worth investigating the `VNA_IOC_SET_NOTIFY_MMIO` handler in the kernel.

### 3. User-Controlled Ring Addresses (Medium)

**Affected struct:** `vioc_ring_init_modern` (lines ~83-91)

```rust
pub struct vioc_ring_init_modern {
    pub rim_index: u16,
    pub rim_qsize: u16,
    pub _pad: [u16; 2],
    pub rim_qaddr_desc: u64,
    pub rim_qaddr_avail: u64,
    pub rim_qaddr_used: u64,
}
```

Three guest physical addresses (`desc`, `avail`, `used`) are passed to the kernel. If the kernel maps these without validating they're within guest memory, this is a **VM escape vector** — the VMM process could point the kernel at host memory, and the kernel would DMA read/write there.

### 4. Unbounded Index Fields (Low)

**Affected:** `rim_index` in `vioc_ring_init_modern`, `rm_index` in `vioc_ring_msi`, `vrs_index` in `vioc_ring_state`

These `u16` index fields select which virtqueue to operate on. If the kernel uses these as array indices without bounds checking against the actual queue count, it's an out-of-bounds access. `VIONA_VQ_MAX = 2` for legacy, but these fields can hold values up to 65535.

### 5. No Invariant Enforcement (Informational)

All struct fields are `pub`, there are no constructors or validation methods, and `Default` is derived freely. This means any caller can construct arbitrary values. This is normal for FFI types, but it means all safety checking must happen elsewhere.

---

## Where to Look Next

This file is the **attack surface definition**, not the attack itself. For the CTF, the exploitable bugs will be in:

1. **The Rust code that calls `ioctl()` with these structs** — look for missing bounds checks on sizes, indexes, or addresses before the syscall
2. **The kernel driver (`viona`)** — look for `copyin`/`copyout` without size validation, or GPA-to-HPA translation without range checks
3. **The code that reads back `vioc_intr_poll_mq`** — if `vipm_nrings` returned by the kernel is trusted and used to index into `vipm_status[16]`, that's an OOB read

I'd recommend searching for the actual ioctl call sites:

```
grep -r "VNA_IOC_SET_PARAMS\|VNA_IOC_RING_INIT\|VNA_IOC_SET_NOTIFY_MMIO" --include="*.rs"
```

**Verdict:** This file is relatively safe on its own — it's just type definitions. The vulnerabilities will be in how these types are constructed, populated, and passed to the kernel.
