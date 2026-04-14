# 24 — API version query failure falls back to hardcoded v18

**Priority:** MEDIUM
**File:** `bin/vmm/src/main.rs:172-178`
**Category:** Silent failure — version mismatch can cause crashes

## Problem

If `machine.hdl().api_version()` fails, the code logs a `warn` and assumes API version 18:

```rust
let api_version = match machine.hdl().api_version() {
    Ok(v) => v,
    Err(e) => {
        warn!(log, "failed to query API version, assuming v18"; "error" => %e);
        18
    }
};
```

The API version determines how VM exit structures are parsed, which ioctls are available, and what capabilities exist. An incorrect version can cause:
- Reading union fields that don't exist (UB)
- Using ioctls the kernel doesn't support (ENOTTY → more silent failures)
- Misinterpreting VM exit data → guest corruption

## Fix

Make this a hard error. If the VMM can't determine the kernel API version, it should not proceed:

```rust
let api_version = machine.hdl().api_version()
    .context("failed to query bhyve API version — cannot safely proceed")?;

if api_version < MIN_SUPPORTED_VERSION {
    anyhow::bail!(
        "kernel bhyve API version {api_version} is below minimum supported ({MIN_SUPPORTED_VERSION})"
    );
}
```

Define `MIN_SUPPORTED_VERSION` as a constant (likely 18, the current `VMM_CURRENT_INTERFACE_VERSION`).
