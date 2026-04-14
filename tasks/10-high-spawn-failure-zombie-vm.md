# 10 — Thread spawn failure silently ignored — VM stuck in zombie state

**Priority:** HIGH
**File:** `bin/vmm/src/control.rs:432, 568`
**Category:** Silent failure — unrecoverable state

## Problem

Migration thread spawn results are discarded with `.ok()`:

```rust
// line ~432 (migrate-source)
thread::Builder::new()
    .name("migrate-source".into())
    .spawn(move || { /* ... */ })
    .ok();  // <-- spawn failure silently ignored

// line ~568 (migrate-dest)
thread::Builder::new()
    .name("migrate-dest".into())
    .spawn(move || { /* ... */ })
    .ok();  // <-- same pattern
```

Before this spawn, the VM state was transitioned to `Migrating` (via `compare_exchange`). If spawn fails (EAGAIN, ENOMEM, rlimit), the state remains `Migrating` but no thread runs. The VM is stuck — subsequent commands check the state and may reject operations because the VM appears to be migrating.

The control socket returns `Response::ok()` with `state: "migrating"` to the client, indicating success.

## Fix

Check the spawn result. On failure, roll back the state and return an error:

```rust
match thread::Builder::new()
    .name("migrate-source".into())
    .spawn(move || { /* ... */ })
{
    Ok(_handle) => {
        Response::ok(name, VmRunState::Migrating)
    }
    Err(e) => {
        // Roll back state
        ctrl.state.store(VmRunState::Running as u8, Ordering::Release);
        Response::err(name, &format!("failed to spawn migration thread: {e}"))
    }
}
```

Apply to both migrate-source (line ~432) and migrate-dest (line ~568).

Note: With the fix from task #06 (passing `Arc<VmController>` into threads), the state rollback can use the Arc directly instead of the raw pointer.
