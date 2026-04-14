# 14 — Widen VirtioDevice trait features to u64

**Spec:** Section 2.2, 6.3 — Feature bits go up to 127+. VIRTIO_F_VERSION_1 is bit 32.
**Severity:** Low (only needed for modern transport, but unblocks it)
**Effort:** ~40 lines across 7 files

## Problem

The VirtioDevice trait uses `u32` for features:

```rust
fn device_features(&self) -> u32;
fn set_features(&self, features: u32);
```

This cannot represent `VIRTIO_F_VERSION_1` (bit 32) or any feature above bit 31. The legacy PCI transport only has a 32-bit feature register, but the trait should be capable of expressing the full feature space so that a future modern transport can use it.

## Files to modify

1. `crates/vmm-devices/src/virtio/mod.rs` — trait definition
2. `crates/vmm-devices/src/virtio/pci.rs` — truncate to u32 for legacy transport
3. `crates/vmm-devices/src/virtio/block.rs` — update impl
4. `crates/vmm-devices/src/virtio/rng.rs` — update impl
5. `crates/vmm-devices/src/virtio/viona.rs` — update impl
6. `crates/vmm-devices/src/virtio/bits.rs` — remove u32 aliases (use u64 directly)

## Changes

### mod.rs — change trait

```rust
pub trait VirtioDevice: Send + Sync + 'static {
    /// Return the device feature bits.
    fn device_features(&self) -> u64;

    /// Called when the guest writes the negotiated features.
    fn set_features(&self, features: u64);

    // ... rest unchanged ...
}
```

### pci.rs — truncate for legacy transport

In `bar_read` (DEVICE_FEATURES):
```rust
bits::LEGACY_REG_DEVICE_FEATURES => {
    // Legacy transport: only the low 32 bits are visible
    self.device.device_features() as u32
}
```

In `bar_write` (GUEST_FEATURES):
```rust
bits::LEGACY_REG_GUEST_FEATURES => {
    // Legacy transport: guest only sees/writes low 32 bits
    let offered = self.device.device_features() as u32;
    let accepted = val & offered;
    vs.guest_features = accepted;
    self.device.set_features(u64::from(accepted));
    // ...
}
```

Also change `vs.guest_features` from `u32` to `u64` in `VirtioPciState`:
```rust
struct VirtioPciState {
    guest_features: u64,  // was u32
    // ...
}
```

### block.rs — update impl

```rust
fn device_features(&self) -> u64 {
    let mut f = bits::VIRTIO_BLK_F_FLUSH
        | bits::VIRTIO_F_RING_EVENT_IDX;
    if self.read_only { f |= bits::VIRTIO_BLK_F_RO; }
    if self.sector_size != 512 { f |= bits::VIRTIO_BLK_F_BLK_SIZE; }
    f
}

fn set_features(&self, features: u64) {
    *self.negotiated_features.lock().expect("lock") = features;
}
```

Note: With u64 features, the `_U32` suffixed constants from task 04 are no longer needed. Use the u64 constants directly.

### rng.rs — update impl

```rust
fn device_features(&self) -> u64 {
    bits::VIRTIO_F_RING_EVENT_IDX
}

fn set_features(&self, _features: u64) {}
```

### viona.rs — update impl

```rust
fn device_features(&self) -> u64 {
    let inner = self.inner.lock().expect("viona lock");
    u64::from(inner.host_features)
        | u64::from(VIRTIO_NET_F_MAC)
        | u64::from(VIRTIO_NET_F_STATUS)
        | bits::VIRTIO_F_RING_EVENT_IDX
}

fn set_features(&self, features: u64) {
    let mut inner = self.inner.lock().expect("viona lock");
    inner.negotiated_features = features as u32;  // kernel API is u32
    // ...
}
```

## Dependencies

- Tasks 02 and 04 should be applied first (feature masking and EVENT_IDX per-device)
- After this change, the `_U32` constants from task 04 are unnecessary

## Test

All existing tests should pass with the type change. Add:

```rust
#[test]
fn u64_features_truncated_by_legacy_transport() {
    // Create a device that offers VIRTIO_F_VERSION_1 (bit 32) + FLUSH
    // Read DEVICE_FEATURES from legacy transport — should only see low 32 bits
    // Verify bit 32 is not visible
}
```

Run: `cargo test -p vmm-devices`
