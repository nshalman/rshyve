# 202 - HIGH - Update VirtioDevice trait to match Propolis patterns

## Problem

vmmnew's `VirtioDevice` trait has several limitations vs Propolis:

1. **32-bit features only** — `device_features() -> u32`. VirtIO 1.0+ uses 64-bit features.
   `VIRTIO_F_VERSION_1` is bit 32, unreachable in current API.
2. **No error returns** — `set_features(u32)` is infallible. Device can't reject bad combos.
3. **Mutable queue slice** — `notify_queue(&self, queues: &mut [VirtQueue])` couples
   notification to queue ownership. Propolis passes `&VirtQueue` (immutable).
4. **No VqChange** — Device isn't notified when queue addresses or interrupt config changes.
   This forces manual `registered_bar` tracking in pci.rs.
5. **No Lifecycle requirement** — Trait doesn't require `Lifecycle`, so devices can skip it.

## Changes

### `crates/vmm-devices/src/virtio/mod.rs`

```rust
pub trait VirtioDevice: Send + Sync + 'static + Lifecycle {
    /// Device feature bits (64-bit for modern transport).
    fn device_features(&self) -> u64;

    /// Called when guest writes negotiated features. Return Err to signal failure.
    fn set_features(&self, features: u64) -> Result<(), ()>;

    /// Read from device-specific configuration space.
    fn cfg_read(&self, offset: u16, len: u8) -> u32;

    /// Write to device-specific configuration space.
    fn cfg_write(&self, offset: u16, val: u32, len: u8);

    /// Handle queue notification. Called with immutable queue reference.
    fn queue_notify(&self, queue_idx: u16, queue: &VirtQueue, physmap: &PhysMap);

    /// Notification of queue configuration change.
    fn queue_change(&self, _queue_idx: u16, _change: VqChange) -> Result<(), ()> {
        Ok(())
    }

    /// Reset the device to initial state.
    fn reset(&self);
}
```

### Migration path (avoid breaking everything at once)

Option A: Change trait in-place, fix all impls in one PR.
Option B: Add `VirtioDeviceV2` trait, migrate devices incrementally.

**Recommended: Option A** — only 4 impls (VirtioBlock, VirtioViona, VirtioRng, test device).

### Per-device changes

**VirtioBlock** (`virtio/block.rs`):
- `device_features() -> u64`: return existing u32 bits as u64 (backward compat)
- `set_features(u64) -> Result`: validate, return Ok
- `notify_queue` → `queue_notify`: adapt signature, move pop/process loop into method
- Add `impl Lifecycle` (see task 200)

**VirtioViona** (`virtio/viona.rs`):
- Same signature changes
- `queue_change(VqChange::Address)`: replace current `queue_addr_set()` logic
- `queue_change(VqChange::IntrCfg)`: update interrupt routing to kernel

**VirtioRng** (`virtio/rng.rs`):
- Trivial signature changes

**VirtioPciDevice** (`virtio/pci.rs`):
- Remove `registered_bar` Mutex — use `queue_change(VqChange::Address)` instead
- On BAR address write: call `device.queue_change(idx, VqChange::Address)`
- On MSI-X vector write: call `device.queue_change(idx, VqChange::IntrCfg)`
- On device reset: call `device.queue_change(idx, VqChange::Reset)` for each queue

## Files to modify

- `crates/vmm-devices/src/virtio/mod.rs` — Trait definition
- `crates/vmm-devices/src/virtio/block.rs` — Adapt impl
- `crates/vmm-devices/src/virtio/viona.rs` — Adapt impl, move queue_addr_set logic
- `crates/vmm-devices/src/virtio/rng.rs` — Adapt impl
- `crates/vmm-devices/src/virtio/pci.rs` — Adapt generic code, remove registered_bar

## Verification

1. `cargo build` / `cargo test` — all existing tests pass
2. Boot VM with virtio-blk + viona — devices still function
3. Verify viona queue setup still works (queue_change replaces queue_addr_set)
