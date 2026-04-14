# Vulnerability Report: `hdl.rs`

## Finding 1 — Integer Overflow in `track_dirty_pages` (Critical)

**Affected:** `track_dirty_pages()`, lines ~280-300

**Root cause:** The bitmap size validation uses arithmetic that wraps on overflow in Rust release builds:

```rust
let pages = (len + 4095) / 4096;          // wraps if len > usize::MAX - 4094
let required_bytes = (pages + 7) / 8;      // then this is 0
if bitmap.len() < required_bytes {          // 0 < 0 is false — check passes
    return Err(...);
}
```

The original `len` (pre-overflow) is passed directly to the kernel:
```rust
vdt_len: len,                              // kernel sees the original huge value
vdt_pfns: bitmap.as_mut_ptr() as *mut _,   // kernel writes into too-small buffer
```

**Exploitation:**
1. Call `track_dirty_pages(0, usize::MAX - 4094, &mut small_buf)`
2. `len + 4095` wraps to `0` → `pages = 0` → `required_bytes = 0`
3. Size check passes with any buffer, even 1 byte
4. Kernel receives `vdt_len = 0xFFFFFFFFFFFFF001` and writes dirty bits far beyond the buffer
5. Heap corruption → arbitrary write primitive

**Fix:** Use `checked_add` / `checked_div` or compute the required size without overflow:
```rust
let pages = len.checked_add(4095).ok_or(...)? / 4096;
```

---

## Finding 2 — Missing Bitmap Validation in `set_dirty_pages` (High)

**Affected:** `set_dirty_pages()`, lines ~310-335

Unlike `track_dirty_pages`, this function performs **zero validation** that `bitmap` is large enough for `len`:

```rust
pub fn set_dirty_pages(&self, gpa: u64, len: u64, bitmap: &[u8]) -> Result<()> {
    // ... API version check only ...
    let mut op = vm_npt_operation {
        vno_len: len,                                   // arbitrary u64
        vno_bitmap: bitmap.as_ptr() as *mut u8,         // tiny buffer
        vno_operation: VNO_OP_SET_DIRTY | VNO_FLAG_BITMAP_IN,
    };
    unsafe { self.inner.ioctl(...) }?;
```

**Exploitation:** Pass `len = 1GB` with a 1-byte bitmap. The kernel reads `ceil(1GB / 4096 / 8) = ~32KB` past the end of the slice. During migration, this leaks heap contents from the VMM process as dirty-page bits, which control which guest memory pages are re-transmitted — an information side-channel that can influence migration behavior.

The `const`-to-`mut` pointer cast (`bitmap.as_ptr() as *mut u8`) is also a soundness hole: if kernel semantics change and it writes through this pointer, it silently mutates data behind an immutable reference.

---

## Finding 3 — TOCTOU Race in `create` (Medium)

**Affected:** `create()`, lines ~58-75

```rust
if opts.force {
    let _ = ctl.vm_destroy(name.as_bytes());   // (1) destroy old VM
}
// ← window: another process creates a VM with the same name
let mut req = vm_create_req::new(name.as_bytes())?;
unsafe { ctl.ioctl(ioctls::VMM_CREATE_VM, &mut req) }?;  // (2) create new VM
// ← window: another process destroys+replaces the VM
let fd = VmmFd::open(name)?;                              // (3) open fd
```

Between steps (2) and (3), an attacker who can create VMs could destroy and replace the target VM. The returned `VmmHdl` then controls a **different VM instance** than the one just created — a confused-deputy scenario.

---

## Finding 4 — Integer Truncation in `set_cpuid` (Low)

**Affected:** `set_cpuid()`, line ~355

```rust
vvcc_nent: entries.len() as u32,   // usize → u32 truncation
```

If `entries.len() > u32::MAX`, the count wraps silently. The kernel sees a small count but the pointer references a much larger array. Practically hard to trigger (requires ~32GB+ allocation for CPUID entries), but violates the safety invariant the `unsafe` ioctl relies on.

---

## Summary

| # | Vulnerability | Severity | Exploitability |
|---|---|---|---|
| 1 | Integer overflow bypasses bitmap size check in `track_dirty_pages` | **Critical** | Heap buffer overflow via kernel write |
| 2 | No bitmap size validation in `set_dirty_pages` | **High** | OOB kernel read, info leak |
| 3 | TOCTOU race in `create` | **Medium** | Confused deputy, requires local attacker |
| 4 | `usize` → `u32` truncation in `set_cpuid` | **Low** | Theoretical, impractical allocation size |

**Primary CTF target: Finding 1.** It's a textbook integer overflow → buffer overflow with a clear exploit path through the kernel ioctl interface.
