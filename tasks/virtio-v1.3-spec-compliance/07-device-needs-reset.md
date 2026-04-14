# 07 — Add DEVICE_NEEDS_RESET signaling mechanism

**Spec:** Section 2.1.2 — Device MAY set DEVICE_NEEDS_RESET (bit 6) on unrecoverable error. MUST send a config change notification when it does.
**Severity:** Low (MAY-level, but good practice)
**Effort:** ~25 lines

## Problem

`bits.rs:22` defines `STATUS_DEVICE_NEEDS_RESET = 64` but it is never set anywhere. When a device backend encounters a fatal error (e.g., backing file I/O failure in virtio-blk, kernel driver crash in viona), there is no mechanism to signal the guest that the device needs a reset.

## Files to modify

1. `crates/vmm-devices/src/virtio/pci.rs` — add method to set DEVICE_NEEDS_RESET
2. `crates/vmm-devices/src/virtio/mod.rs` — extend VirtioDevice trait or add callback
3. `crates/vmm-devices/src/virtio/bits.rs` — already defined, just needs ISR_CFG_CHANGE usage

## Design

The VirtioPciDevice needs a public method that backends can call to signal a fatal error:

### Option A: Shared status reference

Give each backend a reference to the VirtioPciState's status byte so it can set the bit directly. This is complex and requires shared state.

### Option B: Callback (recommended)

Provide a closure to the device backend at construction time:

```rust
// In VirtioPciDevice::new(), create a callback:
let isr_status = Arc::clone(&dev.isr_status_arc);
let intr_pin = dev.intr_pin.clone();
let status = Arc::clone(&dev.virtio_state_arc);

let needs_reset_fn = move || {
    // Set DEVICE_NEEDS_RESET in status
    let mut vs = status.lock().expect("virtio state");
    vs.status |= bits::STATUS_DEVICE_NEEDS_RESET;
    // Fire config change interrupt (ISR bit 1)
    isr_status.fetch_or(bits::ISR_CFG_CHANGE, Ordering::Release);
    drop(vs);
    // Assert interrupt
    if let Some(ref pin) = intr_pin {
        pin.assert();
    }
};
```

This callback can be passed to `VirtioBlock::new()` and stored. When a backing file I/O fails fatally (e.g., ENOSPC, EIO on multiple retries), the backend calls `(self.needs_reset)()`.

### Initial scope

For this task, implement the mechanism and wire it up. Actual error-to-reset triggers in backends can be added incrementally:

1. Add the callback type and plumbing
2. Set DEVICE_NEEDS_RESET from the callback
3. Fire ISR_CFG_CHANGE interrupt
4. Don't add triggers in backends yet (that's per-device work)

## Test

```rust
#[test]
fn device_needs_reset_sets_status_and_isr() {
    // Create VirtioPciDevice
    // Call the needs_reset callback
    // Read status — should have DEVICE_NEEDS_RESET set
    // Read ISR — should have ISR_CFG_CHANGE set
}
```

Run: `cargo test -p vmm-devices`
