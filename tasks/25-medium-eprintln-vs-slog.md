# 25 — Inconsistent error handling: eprintln vs structured logging

**Priority:** MEDIUM
**File:** `bin/vmm/src/control.rs` — 15 `eprintln!` calls across migration handlers
**Category:** Code quality — inconsistent observability

## Problem

The control socket handler uses `slog` structured logging for normal operations, but migration threads spawned from `dispatch()` use `eprintln!` for errors. This means:

- Migration errors go to stderr raw (not structured, not JSON, not captured by log aggregation)
- All other control errors use bunyan-format structured logging
- In production, stderr may be redirected to `/dev/null` or a different location than the structured log

The root cause: the `Logger` is not passed into the spawned migration threads.

## Fix

Pass the logger into spawned threads:

```rust
let log = ctrl.log.clone(); // or pass as parameter
thread::Builder::new()
    .name("migrate-source".into())
    .spawn(move || {
        // Replace all eprintln! with slog macros:
        // Before: eprintln!("[migrate] connect failed: {e}");
        // After:
        slog::error!(log, "migration connect failed"; "target" => &target_addr, "error" => %e);
    })
```

This requires either:
1. Adding a `Logger` field to `VmController`
2. Passing the logger as a parameter through `dispatch()`
3. Using a global logger (not recommended)

Option 1 is cleanest — add `log: Logger` to `VmController` and clone it into each spawned thread.

## Scope

15 `eprintln!` calls to convert, all in `bin/vmm/src/control.rs` within the migrate-source and migrate-dest handlers.
