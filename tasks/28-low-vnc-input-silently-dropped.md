# 28 — VNC keyboard and pointer events silently dropped

**Priority:** LOW
**File:** `crates/vmm-devices/src/fbuf/mod.rs:647, 651`
**Category:** Incomplete feature — silent discard

## Problem

```rust
// TODO: forward to PS/2 keyboard when implemented
// TODO: forward to USB tablet when implemented
```

VNC connections work for display but all keyboard and mouse input is silently discarded. A user who connects via VNC sees the console but cannot type or click.

## Fix

### Phase 1 — Log a warning on first input event

```rust
static WARNED: AtomicBool = AtomicBool::new(false);
if !WARNED.swap(true, Ordering::Relaxed) {
    slog::warn!(log, "VNC input received but keyboard/pointer forwarding not yet implemented");
}
```

### Phase 2 — Wire up keyboard events to PS/2 or USB HID

Connect the `KeyEvent` handler to the xHCI USB tablet/keyboard device:
1. Add a channel from the VNC handler to the xHCI tablet device
2. In `handle_key_event`, translate VNC keysyms to USB HID scancodes
3. In `handle_pointer_event`, translate VNC coordinates to USB tablet absolute coordinates

This is a substantial feature implementation, not a quick fix.
