# 12 — Add config change interrupt support (ISR_CFG_CHANGE)

**Spec:** Section 4.1.4.5 — Device signals configuration space changes via ISR bit 1. Section 3.2.1 — "Device MUST send a configuration change notification when it sets DEVICE_NEEDS_RESET."
**Severity:** Medium (enables live disk resize, link status change)
**Effort:** ~15 lines

## Problem

`ISR_CFG_CHANGE` (bit 1) is defined in `bits.rs:131` but never set. The device has no way to notify the guest that configuration has changed. This blocks:
- Live disk resize (changing `capacity` and notifying guest)
- Network link status changes
- Any dynamic config change

## Files to modify

1. `crates/vmm-devices/src/virtio/pci.rs` — add `signal_config_change()` method
2. `crates/vmm-devices/src/virtio/mod.rs` — optionally add trait method

## Changes

### pci.rs — add public method

```rust
impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// Signal a configuration change to the guest.
    ///
    /// Sets ISR_CFG_CHANGE and asserts the interrupt pin so the
    /// guest re-reads the device configuration space.
    pub fn signal_config_change(&self) {
        self.isr_status.fetch_or(
            bits::ISR_CFG_CHANGE,
            std::sync::atomic::Ordering::Release,
        );
        self.raise_interrupt();
    }
}
```

### Usage example — live disk resize

In a future disk resize path:
```rust
// Update the capacity
self.capacity.store(new_capacity, Ordering::Release);
// Notify guest
pci_device.signal_config_change();
```

### Usage with DEVICE_NEEDS_RESET (task 07)

The DEVICE_NEEDS_RESET callback should call `signal_config_change()` after setting the status bit, per spec section 3.2.1.

## Test

```rust
#[test]
fn config_change_sets_isr_and_interrupts() {
    // Create VirtioPciDevice
    // Call signal_config_change()
    // Read ISR — bit 1 (ISR_CFG_CHANGE) should be set
    // Verify interrupt was asserted
}
```

Run: `cargo test -p vmm-devices`
