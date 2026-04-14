# 06 — 11 identical unsafe raw pointer casts in control.rs

**Priority:** HIGH
**File:** `bin/vmm/src/control.rs:345-346` (repeated 11 times throughout migration dispatch)
**Category:** Unnecessary unsafe, fragile lifetime assumptions

## Problem

The migration dispatch converts a reference to a raw pointer, casts to `usize`, passes it across thread boundaries, and dereferences unsafely:

```rust
let state_ptr = &ctrl.state as *const AtomicU8 as usize;
// ...in spawned thread...
let state = unsafe { &*(state_ptr as *const AtomicU8) };
```

This pattern appears 11 times. The safety comment says "VmController lives for the process lifetime" but the controller is held in an `Arc<VmController>`. If all `Arc` refs are dropped, the pointer dangles. The `dispatch` function receives `&VmController` from the Arc — the spawned thread outlives the borrow.

The `Arc<VmController>` is available in the calling context. There is zero reason not to clone it.

## Fix

Pass an `Arc<VmController>` into each spawned thread:

```rust
// In spawn_control_thread, before the loop:
// ctrl is already Arc<VmController>

// In dispatch():
fn dispatch(ctrl: &Arc<VmController>, cmd: &Command) -> Response {
    // ...
    let ctrl = Arc::clone(ctrl);
    thread::Builder::new()
        .name("migrate-source".into())
        .spawn(move || {
            // Direct access — no unsafe needed
            ctrl.state.store(VmRunState::Running as u8, Ordering::Release);
        })
        .expect("failed to spawn migration thread");
}
```

This eliminates all 11 `unsafe` blocks in the file. The `dispatch` function signature needs to change from `&VmController` to `&Arc<VmController>`.

## Scope

Lines to change (all in `bin/vmm/src/control.rs`):
- 345-346, 358-360, 381-382, 408-409, 421-422 (migrate-source)
- 463-464, 475-476, 497-498, 514-515, 544-545, 557-558 (migrate-dest)
- `dispatch` function signature
- `spawn_control_thread` dispatch call site
