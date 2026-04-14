# 15 — Modern PCI transport (capability structures)

**Spec:** Section 4.1 — Modern virtio-pci uses PCI capabilities for device configuration instead of a flat BAR layout.
**Severity:** HIGH for forward compatibility (legacy is being deprecated)
**Effort:** ~500 lines (major feature)

## Problem

The implementation only supports the legacy PCI transport (flat BAR0 PIO registers). Modern virtio-pci (v1.0+) uses PCI capability structures mapped in MMIO BARs:

- `VIRTIO_PCI_CAP_COMMON_CFG` — device features, status, queue configuration
- `VIRTIO_PCI_CAP_NOTIFY_CFG` — per-queue doorbell registers
- `VIRTIO_PCI_CAP_ISR_CFG` — ISR status
- `VIRTIO_PCI_CAP_DEVICE_CFG` — device-specific configuration
- `VIRTIO_PCI_CAP_PCI_CFG` — PCI config space access

Modern guests (Linux 4.0+, recent Windows virtio drivers) prefer or require the modern transport.

## Design outline

### New file: `crates/vmm-devices/src/virtio/pci_modern.rs`

```rust
pub struct VirtioModernPciDevice<D: VirtioDevice> {
    device: D,
    pci_state: Mutex<DeviceState>,
    common_cfg: Mutex<CommonCfg>,
    isr_status: AtomicU8,
    queues: Vec<Mutex<VirtQueue>>,
    physmap: Arc<PhysMap>,
    intr_pin: Option<Arc<dyn IntrPin>>,
    msix: Option<Arc<MsixTable>>,  // requires task 16
}

struct CommonCfg {
    device_feature_select: u32,   // selects which 32-bit feature word to read
    driver_feature_select: u32,
    driver_features: u64,
    device_status: u8,
    config_generation: u8,
    queue_select: u16,
    // per-queue config
    queue_size: u16,
    queue_msix_vector: u16,
    queue_enable: u16,
    queue_notify_off: u16,
    queue_desc: u64,
    queue_avail: u64,
    queue_used: u64,
}
```

### PCI capability structures

Add virtio PCI capabilities to the config space:
- Each capability is 16+ bytes in PCI config space
- Points to a BAR + offset for the actual register block
- BAR1 (MMIO) for common_cfg, notify, device_cfg
- BAR2 (MMIO) for ISR

### Device IDs

Modern devices use device ID = virtio_device_id + 0x1040:
- Block = 0x1042
- Net = 0x1041
- RNG = 0x1044

### Transitional device

A single device can support BOTH legacy and modern by:
1. Using a transitional device ID (0x1000-0x103F)
2. Having both the legacy BAR0 PIO and modern capability structures
3. The guest driver picks whichever transport it prefers

## Dependencies

- Task 14 (u64 features) must be completed first
- Task 16 (MSI-X) is strongly recommended before this

## Scope

This is a major feature. Break into sub-tasks:
1. Define PCI capability structures and register layout
2. Implement common_cfg read/write handling
3. Implement notify_cfg (doorbell) registers
4. Implement 64-bit queue address configuration
5. Implement 64-bit feature negotiation
6. Wire up to existing VirtioDevice backends
7. Test with Linux guest using `virtio_pci` modern driver

## Test

Boot a Linux guest with `virtio_pci.force_legacy=0` and verify:
- Device discovery via `lspci` shows modern capability structures
- `dmesg` shows virtio-pci modern driver binding
- Block and net devices function correctly
