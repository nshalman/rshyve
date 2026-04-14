# 23 — VmRunState::from_u8 maps unknown values to Stopped

**Priority:** MEDIUM
**File:** `bin/vmm/src/control.rs:64-74`
**Category:** Silent failure — masks state corruption

## Problem

```rust
fn from_u8(v: u8) -> Self {
    match v {
        0 => Self::Running,
        1 => Self::Paused,
        2 => Self::Stopped,
        3 => Self::Migrating,
        _ => Self::Stopped,  // <-- silent corruption masking
    }
}
```

If the `AtomicU8` state value is corrupted (memory bug, cosmic ray, programming error storing an invalid value), the VM silently appears "Stopped" to the control socket. There is even a test (`vm_state_invalid_defaults_to_stopped`) that validates this broken behavior.

## Fix

In debug builds, panic on invalid state. In release builds, log an error:

```rust
fn from_u8(v: u8) -> Self {
    match v {
        0 => Self::Running,
        1 => Self::Paused,
        2 => Self::Stopped,
        3 => Self::Migrating,
        other => {
            debug_assert!(false, "invalid VmRunState: {other}");
            eprintln!("[control] BUG: invalid VmRunState byte: {other}, treating as Stopped");
            Self::Stopped
        }
    }
}
```

Update the test to verify the debug_assert fires (or remove the test for invalid-defaults-to-stopped, since it validates broken behavior).
