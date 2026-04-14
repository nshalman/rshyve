# 05 — Add FEATURES_OK validation on device status write

**Spec:** Section 3.1.1 steps 5-6 — Device must validate the negotiated feature set when driver sets FEATURES_OK. Device clears FEATURES_OK if the set is invalid.
**Severity:** Medium (modern guests use this)
**Effort:** ~15 lines

## Problem

`pci.rs:395-408`:
```rust
bits::LEGACY_REG_DEVICE_STATUS => {
    let new_status = val as u8;
    if new_status == 0 {
        // reset ...
    } else {
        vs.status = new_status;  // stored verbatim
    }
}
```

When the guest writes `STATUS_FEATURES_OK` (0x08), the device should validate that `guest_features` is a valid subset of the offered features. If validation fails, the device must clear the FEATURES_OK bit so the guest reads it back as cleared and knows negotiation failed.

This is technically optional for legacy/transitional devices (spec 3.1.2), but modern guests (Linux 4.0+) do perform this check and expect the device to participate.

## File to modify

`crates/vmm-devices/src/virtio/pci.rs` — `bar_write()`, `LEGACY_REG_DEVICE_STATUS` arm (line 395)

## Change

```rust
bits::LEGACY_REG_DEVICE_STATUS => {
    let new_status = val as u8;
    if new_status == 0 {
        // Reset path (unchanged)
        vs.reset();
        self.device.reset();
        self.isr_status.store(0, std::sync::atomic::Ordering::Relaxed);
        drop(vs);
        self.lower_interrupt();
    } else {
        // Validate features when FEATURES_OK is first set (spec 3.1.1 step 5-6)
        if new_status & bits::STATUS_FEATURES_OK != 0
            && vs.status & bits::STATUS_FEATURES_OK == 0
        {
            let offered = self.device.device_features();
            if vs.guest_features & !offered != 0 {
                // Guest requested features we didn't offer — reject
                vs.status = new_status & !bits::STATUS_FEATURES_OK;
                return;
            }
        }
        vs.status = new_status;
    }
}
```

## Dependencies

- Task 02 (mask guest features) should be applied first. With masking, the validation here becomes a belt-and-suspenders check.
- Task 04 (EVENT_IDX per-device) should be applied first so `device_features()` returns the complete set.

## Test

```rust
#[test]
fn features_ok_rejected_for_invalid_features() {
    // Create device offering features 0x0220 (FLUSH | BLK_SIZE)
    // Write guest_features = 0xFFFF (includes unsupported bits)
    // Write status = ACKNOWLEDGE | DRIVER | FEATURES_OK
    // Read back status — FEATURES_OK should be cleared
}

#[test]
fn features_ok_accepted_for_valid_subset() {
    // Create device offering features 0x0220
    // Write guest_features = 0x0200 (just FLUSH)
    // Write status = ACKNOWLEDGE | DRIVER | FEATURES_OK
    // Read back status — FEATURES_OK should be set
}
```

Run: `cargo test -p vmm-devices`
