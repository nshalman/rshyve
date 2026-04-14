# Vulnerability Analysis: `mem.rs` — Guest Physical Memory Management

## Finding 1: ROM Write-Protection Bypass (High)

**Affected code**: `add_region()` (line ~297) and `lookup()` (line ~335)

### Root Cause

All memory regions — including ROM — are mapped `Prot::RW` into the VMM process, and `SubMapping` inherits this permissive protection. The `RegionKind::Rom` is tracked but **never enforced** on the write path.

```rust
// add_region() — always maps R/W regardless of kind
let mapping = Mapping::new(len, Prot::RW, hdl, offset)?;
```

```rust
// SubMapping::new — inherits prot from Mapping, not from guest policy
fn new(mapping: &Arc<Mapping>) -> Self {
    Self { prot: mapping.prot, .. }  // always Prot::RW
}
```

The `write()` and `write_bytes()` methods check `self.prot.contains(Prot::WRITE)` — which passes for ROM regions. There is no `RegionKind` check anywhere in the data path.

### Exploitation Strategy

A **confused deputy attack** through device emulation:

1. Guest sets up a virtio DMA operation targeting a GPA in the ROM region (e.g., bootrom at `0xFFF00000`).
2. Device emulation calls `mem_ctx.write(gpa, data)` → `lookup()` → `SubMapping::write_bytes()`.
3. All checks pass: the GPA is mapped, prot is R/W, bounds are valid.
4. The VMM writes attacker-controlled data over the bootrom.
5. On next VM reset/reboot, corrupted firmware executes attacker code at the highest privilege level.

### PoC Sketch

```rust
// From a virtio device handler processing guest-supplied scatter-gather:
fn handle_dma_write(mem: &MemCtx, guest_gpa: u64, data: &[u8]) {
    // guest_gpa = 0xFFF00000 (bootrom), data = attacker shellcode
    // This SUCCEEDS — no RegionKind check
    mem.write(guest_gpa, data).unwrap();
}
```

### Fix

`lookup()` should propagate `RegionKind`, or `SubMapping` should carry a read-only flag for ROM regions:

```rust
if region.kind == RegionKind::Rom {
    // Return a SubMapping with Prot::READ only
}
```

---

## Finding 2: TOCTOU / Double-Fetch via Non-Volatile Copy (Medium)

**Affected functions**: `read_bytes()` (line ~230) and `write_bytes()` (line ~248)

### Root Cause

`read<T>()` and `write<T>()` correctly use `read_volatile`/`write_volatile` because guest vCPUs concurrently access the same memory. But `read_bytes()` and `write_bytes()` use `copy_nonoverlapping`, which is **not volatile** and has no atomicity guarantees:

```rust
// read_bytes — NOT volatile, guest can modify mid-copy
unsafe {
    std::ptr::copy_nonoverlapping(
        self.ptr.as_ptr(),   // guest memory — vCPU can write concurrently
        buf.as_mut_ptr(),
        buf.len(),
    );
}
```

### Exploitation Strategy

Classic **double-fetch** against virtio descriptor processing:

1. Guest writes a valid virtio descriptor: `{ addr: 0x1000, len: 64 }`.
2. VMM reads the descriptor with `read_bytes()` (non-atomic copy).
3. A guest vCPU **races** to modify the descriptor mid-copy — e.g., changing `len` to `0xFFFFFFFF` while `addr` is still being copied as the original valid value.
4. VMM gets a torn read: valid `addr` + corrupted `len`.
5. Subsequent DMA using these values causes an **out-of-bounds read/write** in the VMM process.

The race window is small but reliable with a dedicated vCPU spinning on the target cacheline.

### Fix

Use `volatile` byte-by-byte copy or a platform-specific `memcpy_volatile`, and validate all guest-supplied structures **after** copying them to a VMM-private buffer.

---

## Finding 3: `as_ptr()` Returns `*mut` Regardless of Protection (Low)

**Affected function**: `SubMapping::as_ptr()` (line ~145)

```rust
pub unsafe fn as_ptr(&self) -> *mut u8 {
    self.ptr.as_ptr()
}
```

Returns a mutable raw pointer even for read-only mappings. Any caller using this for raw I/O (e.g., `pwrite()` into a ROM SubMapping) bypasses the protection checks in `write()` / `write_bytes()`. This is the `unsafe` escape hatch that makes Finding 1 even easier to hit — callers don't even need to go through the checked write API.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|---|---|---|
| 1 | ROM write-protection bypass — no `RegionKind` enforcement | **High** | Yes — confused deputy via DMA |
| 2 | Non-volatile `copy_nonoverlapping` double-fetch | **Medium** | Yes — vCPU race for torn reads |
| 3 | `as_ptr()` ignores protection flags | **Low** | Amplifier for #1 |

The most exploitable finding is **#1**: any device emulation path that writes to a guest-supplied GPA can corrupt ROM regions. Combined with **#2**, an attacker can race descriptor modifications to achieve out-of-bounds memory access in the VMM process.
