# 02 — Mask guest features to offered subset

**Spec:** Section 2.2 — "A driver MUST NOT accept a feature which the device did not offer."
**Severity:** Medium (correctness)
**Effort:** 3 lines

## Problem

`pci.rs:355-357`:
```rust
bits::LEGACY_REG_GUEST_FEATURES => {
    vs.guest_features = val;
    self.device.set_features(val);
```

The guest's feature selection is stored verbatim. A buggy or malicious guest could "negotiate" features the device never offered, putting the device into an undefined state.

## File to modify

`crates/vmm-devices/src/virtio/pci.rs` — `bar_write()`, the `LEGACY_REG_GUEST_FEATURES` arm (line 355)

## Change

```rust
bits::LEGACY_REG_GUEST_FEATURES => {
    // Mask to features we actually offered (spec 2.2)
    let offered = self.device.device_features()
        | bits::VIRTIO_F_RING_EVENT_IDX as u32;
    let accepted = val & offered;
    vs.guest_features = accepted;
    self.device.set_features(accepted);

    // Propagate event_idx to all queues
    let event_idx = accepted & bits::VIRTIO_F_RING_EVENT_IDX as u32 != 0;
    for q in &mut vs.queues {
        q.set_event_idx(event_idx);
    }
}
```

Note: The `| VIRTIO_F_RING_EVENT_IDX` must match what `bar_read` returns for `DEVICE_FEATURES` at line 294-295. After task 04, the device backends will include EVENT_IDX directly and this OR can be removed from both locations.

## Test

Add a unit test that verifies unsupported feature bits are masked out:
```rust
#[test]
fn guest_features_masked_to_offered() {
    // Create a VirtioPciDevice with known features
    // Write 0xFFFFFFFF to GUEST_FEATURES
    // Read back — should be masked to offered set
}
```

Run: `cargo test -p vmm-devices`
