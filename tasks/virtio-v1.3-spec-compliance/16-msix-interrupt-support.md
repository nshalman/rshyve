# 16 — MSI-X interrupt support for virtio devices

**Spec:** Section 4.1.4.4 — MSI-X vector configuration for virtio-pci devices.
**Severity:** HIGH (required for non-transitional modern devices; large performance win)
**Effort:** ~150 lines

## Problem

All virtio interrupt delivery currently uses legacy INTx — a single shared PCI interrupt line with ISR register polling (`pci.rs:259-280`). This has two costs:

1. **Shared IRQ**: every virtio device shares the same interrupt line, forcing the guest to poll ISR on every device to determine which one fired.
2. **ISR polling overhead**: the guest reads the ISR register (atomic swap + deassert/re-assert dance at `pci.rs:320-335`) on every interrupt, adding a VM exit per notification.

MSI-X eliminates both problems:
- Each virtqueue gets its own interrupt vector — no shared-line polling.
- The device fires the vector directly via `lapic_msi()` — no ISR read needed.
- Required by the spec for non-transitional (modern-only) devices (section 4.1.4.4).

The codebase already has a complete MSI-X table implementation in `crates/vmm-devices/src/pci/msix.rs` (`MsixTable` with `fire()`, `table_read/write()`, `cap_read/write()`, PBA support). This task wires it into the virtio PCI transport.

## Spec rules

1. If MSI-X is enabled, the device MUST NOT use INTx or the ISR register for queue notifications.
2. The driver writes `queue_msix_vector` to assign a vector to each queue. Writing `0xFFFF` means no vector (use INTx fallback).
3. The driver writes `config_msix_vector` to assign a vector for configuration change interrupts.
4. When MSI-X is enabled and the vector for a queue is configured, the device delivers queue interrupts via `MsixTable::fire(vector)` instead of `IntrPin::assert()`.
5. When the driver disables MSI-X or the device resets, revert to INTx delivery.

## Files to modify

1. `crates/vmm-devices/src/virtio/pci.rs` — add MSI-X capability, per-queue vector tracking, interrupt routing
2. `crates/vmm-devices/src/virtio/bits.rs` — add MSI-X related constants
3. `crates/vmm-devices/src/virtio/block.rs` — no changes needed (interrupt callback is already an opaque `Fn()`)

## Changes

### bits.rs — add constants

```rust
/// MSI-X: no vector assigned (use INTx fallback).
pub const VIRTIO_MSI_NO_VECTOR: u16 = 0xFFFF;

/// Legacy transport register offsets when MSI-X is enabled.
/// MSI-X inserts 4 bytes (2x u16 vector fields) after ISR,
/// pushing device config to offset 0x18.
pub const LEGACY_REG_MSIX_CONFIG_VECTOR: u16 = 0x14;
pub const LEGACY_REG_MSIX_QUEUE_VECTOR: u16 = 0x16;
pub const LEGACY_REG_DEVICE_CONFIG_MSIX: u16 = 0x18;

/// Legacy common config size when MSI-X is active.
pub const LEGACY_COMMON_SIZE_MSIX: u16 = 0x18;
```

### pci.rs — VirtioPciState additions

```rust
struct VirtioPciState {
    // ... existing fields ...

    /// MSI-X vector assigned to configuration changes.
    config_msix_vector: u16,
    /// MSI-X vector assigned to each queue.
    queue_msix_vectors: Vec<u16>,
}

impl VirtioPciState {
    fn new(num_queues: usize, queue_size: u16) -> Self {
        // ...
        Self {
            // ... existing ...
            config_msix_vector: bits::VIRTIO_MSI_NO_VECTOR,
            queue_msix_vectors: vec![bits::VIRTIO_MSI_NO_VECTOR; num_queues],
        }
    }

    fn reset(&mut self) {
        // ... existing reset ...
        self.config_msix_vector = bits::VIRTIO_MSI_NO_VECTOR;
        for v in &mut self.queue_msix_vectors {
            *v = bits::VIRTIO_MSI_NO_VECTOR;
        }
    }
}
```

### pci.rs — VirtioPciDevice additions

```rust
pub struct VirtioPciDevice<D: VirtioDevice> {
    // ... existing fields ...

    /// MSI-X table (None if MSI-X not supported).
    msix: Option<Arc<MsixTable>>,
}
```

Add an `msix` parameter to `VirtioPciDevice::new()`:

```rust
pub fn new(
    // ... existing params ...
    msix: Option<Arc<MsixTable>>,
) -> Arc<Self> {
    // If MSI-X is provided, add the capability to PCI config space
    if let Some(ref msix) = msix {
        // Register MSI-X capability in PCI config space at offset 0x40
        // and define BAR4 (MMIO) for the MSI-X table + PBA
        let bar_size = msix.bar_size() as u32;
        pci_state.define_bar(BarN::BAR4, BarDefine::Mmio(bar_size));
        // Add MSI-X capability at config offset 0x40
        pci_state.add_capability(0x40, MSIX_CAP_ID);
    }

    // ...
}
```

### pci.rs — interrupt routing

Replace `raise_queue_interrupt` to check MSI-X first:

```rust
/// Raise a queue interrupt, routing via MSI-X or INTx.
pub fn raise_queue_interrupt(&self, queue_idx: u16) {
    if let Some(ref msix) = self.msix {
        let vs = self.virtio_state.lock().expect("virtio lock");
        let vector = vs.queue_msix_vectors
            .get(queue_idx as usize)
            .copied()
            .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR);
        drop(vs);

        if vector != bits::VIRTIO_MSI_NO_VECTOR {
            msix.fire(vector);
            return;
        }
    }
    // Fallback to legacy INTx
    self.isr_status.fetch_or(
        bits::ISR_QUEUE_INTR,
        std::sync::atomic::Ordering::Release,
    );
    self.raise_interrupt();
}
```

### pci.rs — BAR read/write for MSI-X vector registers

When MSI-X is present, the legacy BAR0 layout shifts device-specific config from 0x14 to 0x18 and inserts vector registers at 0x14-0x17:

```rust
fn device_config_offset(&self) -> u16 {
    if self.msix.is_some() {
        bits::LEGACY_REG_DEVICE_CONFIG_MSIX
    } else {
        bits::LEGACY_REG_DEVICE_CONFIG
    }
}

fn bar_read(&self, offset: u16, len: usize) -> u32 {
    let dev_cfg_start = self.device_config_offset();
    if offset >= dev_cfg_start {
        let dev_offset = offset - dev_cfg_start;
        return self.device.cfg_read(dev_offset, len as u8);
    }

    match offset {
        // ... existing registers at 0x00-0x13 unchanged ...

        bits::LEGACY_REG_MSIX_CONFIG_VECTOR if self.msix.is_some() => {
            let vs = self.virtio_state.lock().expect("lock");
            u32::from(vs.config_msix_vector)
        }
        bits::LEGACY_REG_MSIX_QUEUE_VECTOR if self.msix.is_some() => {
            let vs = self.virtio_state.lock().expect("lock");
            let sel = vs.queue_select as usize;
            u32::from(
                vs.queue_msix_vectors.get(sel).copied()
                    .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR)
            )
        }
        _ => 0,
    }
}

fn bar_write(&self, offset: u16, val: u32, len: usize) {
    let dev_cfg_start = self.device_config_offset();
    if offset >= dev_cfg_start {
        let dev_offset = offset - dev_cfg_start;
        self.device.cfg_write(dev_offset, val, len as u8);
        return;
    }

    match offset {
        // ... existing registers unchanged ...

        bits::LEGACY_REG_MSIX_CONFIG_VECTOR if self.msix.is_some() => {
            let mut vs = self.virtio_state.lock().expect("lock");
            vs.config_msix_vector = val as u16;
        }
        bits::LEGACY_REG_MSIX_QUEUE_VECTOR if self.msix.is_some() => {
            let mut vs = self.virtio_state.lock().expect("lock");
            let sel = vs.queue_select as usize;
            if let Some(v) = vs.queue_msix_vectors.get_mut(sel) {
                *v = val as u16;
            }
        }
        _ => {}
    }
}
```

### pci.rs — BAR4 handler for MSI-X table/PBA access

```rust
fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
    match bar {
        BarN::BAR0 => { /* existing legacy config handling */ }
        BarN::BAR4 if self.msix.is_some() => {
            let msix = self.msix.as_ref().unwrap();
            match rwo {
                RWOp::Read(ro) => {
                    let val = msix.table_read(offset);
                    match ro.len() {
                        1 => ro.write_u8(val as u8),
                        2 => ro.write_u16(val as u16),
                        4 => ro.write_u32(val),
                        _ => ro.write_u32(val),
                    }
                }
                RWOp::Write(wo) => {
                    let val = match wo.len() {
                        1 => u32::from(wo.read_u8()),
                        2 => u32::from(wo.read_u16()),
                        4 => wo.read_u32(),
                        _ => wo.read_u32(),
                    };
                    msix.table_write(offset, val);
                }
            }
        }
        _ => {}
    }
}
```

## Dependencies

- Task 15 (modern PCI transport) is recommended but not required. MSI-X works with legacy transport too — the Linux `virtio_pci_legacy` driver supports MSI-X when the capability is present.
- The `MsixTable` in `crates/vmm-devices/src/pci/msix.rs` is already complete. No changes needed there.

## Test

```rust
#[test]
fn msix_vector_assignment_readback() {
    // Create device with MSI-X enabled (2 vectors)
    // Write queue_select = 0, then write queue_msix_vector = 1
    // Read queue_msix_vector back — should be 1
    // Write queue_select = 1, read queue_msix_vector — should be 0xFFFF (default)
}

#[test]
fn msix_config_vector_readback() {
    // Write config_msix_vector = 0
    // Read back — should be 0
    // Reset device — should revert to 0xFFFF
}

#[test]
fn msix_shifts_device_config_offset() {
    // Create device with MSI-X
    // Read device config at offset 0x18 (not 0x14)
    // Verify it returns device-specific data
}

#[test]
fn msix_no_vector_falls_back_to_intx() {
    // Create device with MSI-X but leave vector as 0xFFFF
    // Trigger a queue interrupt
    // Verify ISR register is set (INTx path used)
}
```

Run: `cargo test -p vmm-devices -- pci`
