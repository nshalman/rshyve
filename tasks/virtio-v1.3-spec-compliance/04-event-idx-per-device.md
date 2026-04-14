# 04 — Move EVENT_IDX from transport to device backends

**Spec:** Section 2.2 — "A device MUST NOT offer features it wouldn't support if accepted."
**Severity:** Medium (architectural violation, currently benign)
**Effort:** ~15 lines across 5 files

## Problem

`pci.rs:294-295`:
```rust
bits::LEGACY_REG_DEVICE_FEATURES => {
    self.device.device_features()
        | bits::VIRTIO_F_RING_EVENT_IDX as u32
}
```

The PCI transport layer injects `VIRTIO_F_RING_EVENT_IDX` into every device's feature set. The spec says devices must only offer features they support. The feature should be offered by each backend that supports it.

## Files to modify

1. `crates/vmm-devices/src/virtio/pci.rs` — remove hardcoded OR
2. `crates/vmm-devices/src/virtio/block.rs` — add EVENT_IDX to device_features()
3. `crates/vmm-devices/src/virtio/rng.rs` — add EVENT_IDX to device_features()
4. `crates/vmm-devices/src/virtio/viona.rs` — add EVENT_IDX to device_features()
5. `crates/vmm-devices/src/virtio/bits.rs` — add a u32 constant for convenience

## Changes

### bits.rs — add 32-bit constant

The existing constant is `u64` (for future modern transport). Add a `u32` alias for legacy transport use:

```rust
/// RING_EVENT_IDX as u32 for legacy transport feature advertisement.
pub const VIRTIO_F_RING_EVENT_IDX_U32: u32 = 1 << 29;
```

### pci.rs:294-295 — remove the OR

```rust
// Before:
self.device.device_features()
    | bits::VIRTIO_F_RING_EVENT_IDX as u32

// After:
self.device.device_features()
```

### pci.rs:355-361 — update masking (after task 02)

If task 02 was applied, the masking OR also needs to be removed:
```rust
// Before:
let offered = self.device.device_features()
    | bits::VIRTIO_F_RING_EVENT_IDX as u32;

// After:
let offered = self.device.device_features();
```

### block.rs:188-192 — add EVENT_IDX

```rust
fn device_features(&self) -> u32 {
    let mut f = bits::VIRTIO_BLK_F_FLUSH as u32
        | bits::VIRTIO_F_RING_EVENT_IDX_U32;
    if self.read_only { f |= bits::VIRTIO_BLK_F_RO as u32; }
    if self.sector_size != 512 { f |= bits::VIRTIO_BLK_F_BLK_SIZE as u32; }
    f
}
```

### rng.rs:29-31 — add EVENT_IDX

```rust
fn device_features(&self) -> u32 {
    bits::VIRTIO_F_RING_EVENT_IDX_U32
}
```

### viona.rs — add EVENT_IDX

The viona device's `device_features()` reads features from the kernel driver. EVENT_IDX should be included if the kernel viona driver supports it. Check if the kernel-returned features already include bit 29. If not, OR it in:

```rust
fn device_features(&self) -> u32 {
    let inner = self.inner.lock().expect("viona lock");
    inner.host_features | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS
        | bits::VIRTIO_F_RING_EVENT_IDX_U32
}
```

## Test

After this change, all existing functionality should be identical. The feature advertisement path hasn't changed — just WHO advertises EVENT_IDX.

Run: `cargo test -p vmm-devices` and verify a guest still negotiates EVENT_IDX (boot a Linux VM, check `virtio_pci` driver log).
