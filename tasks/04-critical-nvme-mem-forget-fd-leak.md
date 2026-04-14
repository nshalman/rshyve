# 04 — NVMe `mem::forget(file)` leaks fd permanently

**Priority:** CRITICAL
**File:** `crates/vmm-devices/src/nvme/mod.rs:295`
**Category:** Resource leak, potential data corruption

## Problem

```rust
std::mem::forget(file);
```

The NVMe controller leaks the `File` object to keep the raw fd alive for worker threads. Problems:

1. **The fd is never closed**, even when `NvmeController` is dropped. In a long-running VMM with device hot-plug or migration teardown, this leaks fds.

2. **Use-after-close risk:** If the `NvmeController` is dropped and the OS reuses that fd number for a new file (e.g., a different disk image), the orphaned worker threads (if any survive the drop) will pread/pwrite to the wrong file. This is a data corruption vector.

3. **Inconsistent with VirtioBlock:** `VirtioBlock` at line ~98 correctly stores `_file: File` to keep the file alive via ownership. The NVMe controller should do the same.

## Fix

Store the `File` in the `NvmeController` struct:

```rust
pub struct NvmeController {
    state: Mutex<NvmeState>,
    _file: File,  // Keep alive for raw fd used by workers
    // ...
}
```

In the constructor, remove `std::mem::forget(file)` and store it:

```rust
// Remove: std::mem::forget(file);
// ...
NvmeController {
    state: Mutex::new(state),
    _file: file,
    // ...
}
```

The raw fd extracted before this point (`file.as_raw_fd()`) remains valid as long as `_file` lives. When `NvmeController` is dropped, `_file` is dropped, which closes the fd. Worker threads must be joined or signaled before the controller is dropped (they already should be via channel close).

## Testing

- Verify fd is closed after NvmeController drop (check `/proc/self/fd` or use `fcntl(fd, F_GETFD)`)
- Verify workers receive channel-closed error when controller is dropped
