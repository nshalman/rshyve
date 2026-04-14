# Vulnerability Report: `crates/viona-api/src/lib.rs`

## 1. Kernel NULL-Pointer Dereference via Unvalidated `link_id` (Critical)

**Affected function:** `VionaFd::new()` — line 21  
**Type:** Missing input validation leading to kernel panic (DoS / potential privilege escalation)

### Root Cause

```rust
pub fn new(link_id: u32, vm_fd: RawFd) -> Result<Self> {
    let this = Self::open()?;
    let mut vna_create = vioc_create { c_linkid: link_id, c_vmfd: vm_fd };
    let _ = unsafe { this.ioctl(VNA_IOC_CREATE, &mut vna_create) }?;
    Ok(this)
}
```

`link_id` is a `u32` passed directly to the kernel via `VNA_IOC_CREATE` with **zero validation**. The kernel's `mac` module dereferences a pointer derived from this ID without a NULL check. Passing an invalid link ID triggers:

```
BAD TRAP: type=e (#pf Page fault) addr=110 occurred in module "mac"
```

This is a **confirmed kernel panic** — NULL pointer dereference in the `mac` module when the datalink ID doesn't map to a valid link.

### Exploitation Strategy

1. Open `/dev/viona` (requires access to the device node)
2. Call `VNA_IOC_CREATE` with a crafted `c_linkid` that doesn't correspond to any real datalink
3. Kernel panics — instant system DoS

```rust
// PoC sketch
let viona = VionaFd::open().unwrap();
let mut create = vioc_create { c_linkid: 0xDEADBEEF, c_vmfd: vm_fd };
unsafe { viona.ioctl(VNA_IOC_CREATE, &mut create) }; // kernel panic
```

**Impact:** Any process with access to `/dev/viona` can crash the entire system. On a multi-tenant hypervisor, one tenant's VMM can take down all VMs.

---

## 2. Use-After-Destroy Race via Safe `&self` API (High)

**Affected functions:** `ioctl_usize()` (line 45), `ioctl_usize_safe()` (line 76)  
**Type:** TOCTOU / Use-after-destroy race condition

### Root Cause

`VionaFd` is a newtype around `File`, which auto-derives `Sync`. All ioctl methods take `&self` (shared reference). The `ioctl_usize_safe` whitelist includes `VNA_IOC_DELETE`:

```rust
pub fn ioctl_usize(&self, cmd: i32, data: usize) -> Result<i32> {  // &self, not &mut self
    if !Self::ioctl_usize_safe(cmd) { ... }
    unsafe { ioctl(self.as_raw_fd(), cmd, data as *mut libc::c_void) }
}
```

Since `VionaFd: Sync` and methods use `&self`, multiple threads can issue ioctls concurrently without any Rust-level synchronization:

```rust
// Thread A:
viona.ioctl_usize(VNA_IOC_DELETE, 0)?;      // destroys kernel state

// Thread B (concurrent):
viona.ioctl_usize(VNA_IOC_RING_KICK, 0)?;   // operates on destroyed state
```

### Exploitation Strategy

1. Share an `Arc<VionaFd>` between two threads
2. Thread A calls DELETE to destroy the kernel-side viona instance
3. Thread B races to call RING_KICK, RING_RESET, or other operations on the now-destroyed instance
4. If the kernel doesn't perfectly serialize all operations against the destroyed-state check, this is a use-after-free in kernel memory

**Impact:** Kernel memory corruption, potential privilege escalation.

---

## 3. Soundness Hole in `ioctl_usize_safe` Whitelist (Medium)

**Affected function:** `ioctl_usize_safe()` — line 76  
**Type:** Unsafe code behind safe API boundary (soundness bug)

### Root Cause

The whitelist includes three GET operations:

```rust
VNA_IOC_GET_MTU       // 0x27
VNA_IOC_GET_PAIRS     // 0x30
VNA_IOC_GET_USEPAIRS  // 0x32
```

The ioctl encoding (`V << 16 | C << 8 | num`) contains **no direction bits** — unlike standard `_IOR`/`_IOW` macros, there's no way to programmatically verify whether a command performs copyout. The safety claim in the comment ("known to not require any copyin/copyout") is based solely on manual knowledge.

If **any** of these GET ioctls perform copyout (kernel writes to the pointer argument), then `ioctl_usize` becomes an arbitrary kernel write primitive accessible from **safe Rust code**:

```rust
// If GET_MTU does copyout, this writes the MTU value to address 0x41414141
viona.ioctl_usize(VNA_IOC_GET_MTU, 0x41414141)?;
```

**Impact:** If the whitelist is wrong for even one entry, safe Rust code can trigger arbitrary kernel writes.

---

## 4. `RawFd` Lifetime Safety Gap (Medium)

**Affected function:** `VionaFd::new()` — line 21  
**Type:** TOCTOU on file descriptor

### Root Cause

```rust
pub fn new(link_id: u32, vm_fd: RawFd) -> Result<Self> {
```

`vm_fd: RawFd` is just an `i32` with no lifetime or ownership guarantee. Between the caller obtaining the fd and the kernel processing `VNA_IOC_CREATE`, the fd could be closed and reassigned to a different resource by another thread. The kernel's `fget()` call would then associate the viona instance with the wrong VM.

This is a known Rust anti-pattern — `RawFd` should be `BorrowedFd<'_>` to get lifetime guarantees.

**Impact:** VM/VMM confusion — viona instance bound to wrong VM, potential cross-VM data leak.

---

## Summary

| # | Vulnerability | Severity | Exploitable? |
|---|---|---|---|
| 1 | Kernel panic via invalid `link_id` | **Critical** | Yes — confirmed crash |
| 2 | Use-after-destroy race (`&self` + DELETE) | **High** | Likely — depends on kernel serialization |
| 3 | Whitelist soundness (GET ioctls) | **Medium** | Conditional — needs kernel source verification |
| 4 | `RawFd` TOCTOU | **Medium** | Theoretical — tight race window |

The critical finding (#1) is a **trivially exploitable kernel DoS** requiring only access to `/dev/viona`.
