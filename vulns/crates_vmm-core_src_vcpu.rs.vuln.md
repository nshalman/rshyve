# Vulnerability Analysis: `vcpu.rs`

## Finding 1: Data Race on vCPU Entry/Exit — Missing `Sync` Guard

**Severity: High**
**Type: Race condition (CWE-362)**
**Affected: All methods, especially `enter()` (line ~237)**

### Root Cause

`Vcpu` contains `i32` + `Arc<VmmHdl>` — both `Send + Sync` — so Rust auto-derives `Send + Sync` for `Vcpu`. Every method takes `&self` (shared reference), not `&mut self`. This means multiple threads can concurrently call `enter()`, `set_reg()`, etc. on the **same vCPU** with no Rust-level synchronization.

The code comments on lines 14–16 acknowledge this:

> *"This type does not enforce that invariant in Rust — the caller must ensure single-threaded access per vCPU."*

But there is no `!Sync` marker, no `Mutex`, no `Cell`/`RefCell`, and nothing preventing safe Rust from violating the invariant.

### Exploitation Strategy

```rust
use std::sync::Arc;

// Attacker creates two handles to the same vCPU
let vcpu = Arc::new(Vcpu::new_for_thread(0, hdl.clone())); // pub fn, any caller

let v1 = vcpu.clone();
let v2 = vcpu.clone();

// Thread A: enter guest, guest performs MMIO write
let t1 = std::thread::spawn(move || {
    let exit = v1.enter(&VmEntry::Run, api_ver).unwrap();
    // exit is MMIO write — needs handling before re-entry
    // But thread B re-enters before we handle it...
    handle_mmio(&exit);
    v1.enter(&VmEntry::MmioFulfill(...), api_ver) // stale fulfillment
});

// Thread B: races to re-enter the guest immediately  
let t2 = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_micros(10));
    // Re-enters guest while Thread A's exit is unhandled
    let exit = v2.enter(&VmEntry::Run, api_ver).unwrap();
    // Guest observes partially-handled device state
});
```

**Impact:** A malicious guest can trigger carefully-timed I/O exits. If the host VMM has a bug where two threads race on the same vCPU, the guest can:
- Get I/O fulfillments meant for a different exit (confused deputy)
- Cause the host to write to stale device state
- Bypass I/O interposition checks (the exit-handling thread thinks it's responding to exit X, but the guest has already moved past it)

### Fix

```rust
// Add negative trait impl to prevent sharing across threads:
impl !Sync for Vcpu {}
// Or wrap the entry point in a Mutex<()> guard
```

---

## Finding 2: Unbounded vCPU ID Passed to Kernel ioctls

**Severity: Medium**
**Type: Improper input validation (CWE-20)**
**Affected: `new()` (line 39), `new_for_thread()` (line 47), all ioctl call sites**

### Root Cause

The `id` field is `i32` with no bounds check. `new_for_thread()` is `pub` — any caller can construct a `Vcpu` with an arbitrary ID (negative, `i32::MAX`, etc.) that is passed directly into kernel ioctl structs:

```rust
// Line 47 — public, no validation
pub fn new_for_thread(id: i32, hdl: Arc<VmmHdl>) -> Self {
    Self { id, hdl }
}

// Then used directly in kernel structs:
let mut vr = vm_register {
    cpuid: self.id,  // arbitrary i32 → kernel
    ...
};
```

### Exploitation Strategy

If the kernel indexes a per-vCPU array using this ID without bounds checking (common in bhyve — `vcpuid` is often used as an array index into `vm->vcpu[]`), a negative or out-of-bounds ID could:
- Read/write adjacent kernel memory (OOB array access)
- Trigger kernel panic (null deref if the vCPU slot is unallocated)

```rust
// Craft a Vcpu with out-of-bounds ID
let evil = Vcpu::new_for_thread(-1, hdl.clone());
evil.set_reg(VM_REG_GUEST_RAX, 0x4141414141414141)?;
// Kernel does: vm->vcpu[-1].regs[RAX] = 0x41... → OOB write
```

### Fix

```rust
pub fn new_for_thread(id: i32, hdl: Arc<VmmHdl>) -> Result<Self, Error> {
    if id < 0 || id >= MAX_VCPUS as i32 {
        return Err(Error::InvalidVcpuId(id));
    }
    Ok(Self { id, hdl })
}
```

---

## Finding 3: Non-Atomic BSP Initialization

**Severity: Low**
**Type: Inconsistent state on partial failure (CWE-460)**
**Affected: `setup_bsp()` (line ~219)**

```rust
pub fn setup_bsp(&self) -> std::io::Result<()> {
    self.reboot_state()?;          // Step 1: reset all state
    self.set_run_state(VRS_RUN)?;  // Step 2: mark runnable
    self.set_reg(VM_REG_GUEST_RIP, 0xfff0)?;  // Step 3: set RIP
    Ok(())
}
```

If step 2 succeeds but step 3 fails, the vCPU is in `VRS_RUN` state with RIP at the post-reset default (0x0000FFF0 from reset, but CS base is 0xFFFF0000). The caller gets an `Err` but the vCPU is partially initialized and **runnable**. If the caller doesn't handle the error by tearing down the vCPU, a subsequent `enter()` call runs the guest from an unintended address.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|---|---|---|
| 1 | Race condition — `Vcpu` is `Sync` with `&self` methods, no mutual exclusion | **High** | Yes — guest-triggered I/O races |
| 2 | Unbounded vCPU ID to kernel ioctls | **Medium** | Depends on kernel validation |
| 3 | Partial BSP init on failure | **Low** | Edge case, needs error mishandling |

The primary vulnerability is **#1** — the data race. It's a textbook case of a documented-but-unenforced safety invariant in unsafe Rust FFI code. For a CTF, exploiting the race to cause confused I/O fulfillment (or combining it with #2 to target an OOB vCPU slot) would be the intended path.
