# 22 — Duplicate `cache_api_version` — identical function in two crates

**Priority:** MEDIUM
**Files:** `crates/bhyve-api/src/lib.rs:495-520`, `crates/viona-api/src/lib.rs:199-224`
**Category:** Code quality — DRY violation

## Problem

`cache_api_version` is character-for-character identical in both crates. Same logic, same comments, same structure. Both parse `/etc/versions/bhyve` or check an API version ioctl.

## Fix

Option A — Extract to a shared utility crate:

Create `crates/vmm-util/src/lib.rs`:

```rust
/// Query and cache the kernel API version for a VMM device.
///
/// Checks the version file first, falls back to ioctl.
pub fn cache_api_version(
    fd: std::os::unix::io::RawFd,
    ioctl_cmd: u64,
    version_file: &str,
) -> Option<u32> {
    // ... shared implementation ...
}
```

Option B — Since both crates already depend on `libc`, put the shared function in `bhyve-api` and have `viona-api` depend on `bhyve-api` (if it doesn't already). This is simpler but creates a dependency that may not be desired.

Option C — Accept the duplication but add a comment linking the two copies so they stay in sync. Least effort, but doesn't fix the underlying issue.

Recommend option A for clean architecture.
