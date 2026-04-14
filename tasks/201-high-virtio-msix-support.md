# 201 - HIGH - MSI-X support for virtio devices

## Problem

All virtio queues share a single INTx pin (legacy PCI interrupt). Every queue
completion triggers the same interrupt line, causing contention and unnecessary
guest exits. This is the single biggest performance gap vs Propolis.

vmmnew already has `MsixTable` infrastructure in `crates/vmm-devices/src/pci/msix.rs`
(NVMe and passthru use it), but `VirtioPciDevice` doesn't wire it up.

## Design

### New types in `crates/vmm-devices/src/virtio/mod.rs`

```rust
/// Per-queue interrupt callback.
pub trait VirtioIntr: Send + 'static {
    fn notify(&self);
    fn read(&self) -> VqIntr;
}

/// Interrupt info returned by VirtioIntr::read().
pub enum VqIntr {
    Pin,                        // Legacy INTx
    Msi(u64, u32, bool),        // addr, data, masked
}

/// Queue configuration change notification.
pub enum VqChange {
    Reset,
    Address,
    IntrCfg,  // MSI-X vector assignment changed
}
```

### Changes to `VirtioPciDevice<D>` in `crates/vmm-devices/src/virtio/pci.rs`

1. Add `msix: Option<Arc<MsixTable>>` field
2. Add `msix_queue_vec: Mutex<Vec<u16>>` — maps queue index to MSI-X vector
3. Add MSI-X capability to PCI config space (cap ID 0x11) at a fixed offset (e.g., 0x40)
4. Allocate MSI-X BAR (BAR1, MMIO) for table + PBA
5. On `cfg_write` to MSI-X capability: toggle enable/mask bits via `MsixTable::cap_write()`
6. On queue interrupt: check if MSI-X enabled → fire MSI-X vector; else fall back to INTx

### Interrupt delivery path

```
VirtioCompletion::complete()
  → should_interrupt() = true
    → interrupt callback
      → if msix.enabled && !masked:
          msix.fire(queue_vec[queue_idx])   // per-queue MSI-X
        else:
          isr_status |= ISR_QUEUE_INTR
          intr_pin.pulse()                  // legacy fallback
```

### Per-queue vector assignment

Virtio spec (VirtIO 1.2 sec 4.1.4.3.2): the driver writes a 16-bit MSI-X vector
to the Queue MSI-X Vector register. In legacy transport, this is at BAR0 offset 0x16
(VIRTIO_PCI_QUEUE_SEL=0x0E selects queue, then 0x16 reads/writes vector).

Add handling in `bar_rw()` for offset 0x16:
- Write: store vector in `msix_queue_vec[selected_queue]`
- Read: return current vector for selected queue
- Special value 0xFFFF = NO_VECTOR (no MSI-X for this queue)

### Config change vector

BAR0 offset 0x14 in legacy: MSI-X vector for config change interrupts.
Store separately as `msix_config_vec: Mutex<u16>`.

## Files to modify

- `crates/vmm-devices/src/virtio/mod.rs` — Add VirtioIntr, VqIntr, VqChange
- `crates/vmm-devices/src/virtio/pci.rs` — Wire MsixTable into VirtioPciDevice
- `crates/vmm-devices/src/virtio/queue.rs` — Store `Option<Box<dyn VirtioIntr>>` per queue
- `crates/vmm-devices/src/pci/msix.rs` — Possibly extend with `is_enabled()` / `is_masked(vec)` helpers

## Existing infra to reuse

- `crates/vmm-devices/src/pci/msix.rs:60` — `MsixTable` with `fire()`, `cap_read()`, `cap_write()`, `bar_read()`, `bar_write()`
- `crates/vmm-devices/src/nvme/mod.rs` — Reference for how NVMe wires up MSI-X

## Propolis reference

- `lib/propolis/src/hw/virtio/pci.rs:305-347` — MSI-X integration
- `lib/propolis/src/hw/virtio/mod.rs:232-255` — VirtioIntr trait

## Verification

1. Boot Linux guest with virtio-blk, verify MSI-X vectors in `lspci -vv`
2. `dtrace -n 'vmm$target:::msix_fire { printf("vec=%d", arg0) }' -p $PID`
3. Benchmark: `fio --bs=4k --iodepth=32 --rw=randread` before/after MSI-X
4. Verify INTx fallback works when guest doesn't enable MSI-X (e.g., older kernels)
