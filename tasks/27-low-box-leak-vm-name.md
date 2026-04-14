# 27 — Box::leak for VM name string

**Priority:** LOW
**File:** `bin/vmm/src/control.rs:99`
**Category:** Code quality — intentional memory leak

## Problem

```rust
let name: &'static str = Box::leak(vm_name.into_boxed_str());
```

This leaks heap memory to create a `'static` lifetime for the VM name. The `Response` struct uses `&'static str` for the name field. While the comment says "the controller lives for the process lifetime," this prevents clean shutdown-and-recreate and is a code smell.

## Fix

Store the name as a `String` in `VmController` and use `String` (or `Cow<'_, str>`) in `Response`:

```rust
pub struct VmController {
    name: String,
    // ...
}

#[derive(Serialize)]
struct Response {
    success: bool,
    name: String,  // was &'static str
    // ...
}
```

Or use `Arc<str>` if the name needs to be cheaply cloned:

```rust
pub struct VmController {
    name: Arc<str>,
    // ...
}
```

The `serde::Serialize` derive works with all of these types.
