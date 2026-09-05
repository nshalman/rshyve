// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the virtio PCI transport.

use super::*;

/// Minimal test VirtIO device for transport tests.
struct TestVirtioDevice {
    features: u64,
}

impl VirtioDevice for TestVirtioDevice {
    fn device_features(&self) -> u64 {
        self.features
    }

    fn set_features(&self, _features: u64) {}

    fn cfg_read(&self, offset: u16, _len: u8) -> u32 {
        // Return offset as value for testability
        u32::from(offset)
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        0
    }

    fn reset(&self) {}
}

impl Lifecycle for TestVirtioDevice {
    fn type_name(&self) -> &'static str {
        "test-virtio"
    }
}

fn make_test_device() -> Arc<VirtioPciDevice<TestVirtioDevice>> {
    make_device_of_type(bits::VIRTIO_DEV_TYPE_BLOCK)
}

fn make_device_of_type(
    dev_type: u16,
) -> Arc<VirtioPciDevice<TestVirtioDevice>> {
    let physmap = Arc::new(PhysMap::new());
    let bus_pio = Arc::new(PioBus::new());
    let bus_mmio = Arc::new(MmioBus::new());
    VirtioPciDevice::new(
        TestVirtioDevice {
            features: 0x42
                | bits::VIRTIO_F_RING_EVENT_IDX
                | bits::VIRTIO_F_RING_INDIRECT_DESC,
        },
        dev_type,
        1,
        256,
        8, // 8 bytes of device config
        None,
        physmap,
        bus_pio,
        bus_mmio,
        None, // no MSI-X
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RestoreRingCall {
    queue_idx: u16,
    size: u16,
    desc: u64,
    avail: u64,
    used: u64,
    avail_idx: u16,
    used_idx: u16,
    msix_addr: u64,
    msix_data: u32,
}

#[derive(Debug, Default)]
struct TrackingVirtioDeviceState {
    set_features: Vec<u64>,
    notify_addrs: Vec<(u16, u64)>,
    restored_rings: Vec<RestoreRingCall>,
    reset_all_rings: usize,
}

struct TrackingVirtioDevice {
    features: u64,
    state: Mutex<TrackingVirtioDeviceState>,
}

impl VirtioDevice for TrackingVirtioDevice {
    fn device_features(&self) -> u64 {
        self.features
    }

    fn set_features(&self, features: u64) {
        self.state
            .lock()
            .expect("tracking state lock")
            .set_features
            .push(features);
    }

    fn cfg_read(&self, offset: u16, _len: u8) -> u32 {
        u32::from(offset)
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        0
    }

    fn reset_all_rings(&self) {
        self.state
            .lock()
            .expect("tracking state lock")
            .reset_all_rings += 1;
    }

    fn restore_ring_state(
        &self,
        queue_idx: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
        avail_idx: u16,
        used_idx: u16,
        msix_addr: u64,
        msix_data: u32,
    ) {
        self.state
            .lock()
            .expect("tracking state lock")
            .restored_rings
            .push(RestoreRingCall {
                queue_idx,
                size,
                desc,
                avail,
                used,
                avail_idx,
                used_idx,
                msix_addr,
                msix_data,
            });
    }

    fn set_notify_addrs(&self, pio_port: u16, mmio_addr: u64) {
        self.state
            .lock()
            .expect("tracking state lock")
            .notify_addrs
            .push((pio_port, mmio_addr));
    }

    fn reset(&self) {}
}

impl Lifecycle for TrackingVirtioDevice {
    fn type_name(&self) -> &'static str {
        "tracking-virtio"
    }
}

fn make_tracking_device() -> Arc<VirtioPciDevice<TrackingVirtioDevice>> {
    let physmap = Arc::new(PhysMap::new());
    let bus_pio = Arc::new(PioBus::new());
    let bus_mmio = Arc::new(MmioBus::new());
    VirtioPciDevice::new(
        TrackingVirtioDevice {
            features: bits::VIRTIO_F_VERSION_1,
            state: Mutex::new(TrackingVirtioDeviceState::default()),
        },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        physmap,
        bus_pio,
        bus_mmio,
        None,
    )
}

#[test]
fn pci_identity() {
    let dev = make_test_device();
    // Vendor ID
    let id = dev.cfg_read(0x00, 4);
    assert_eq!(id & 0xFFFF, bits::VIRTIO_PCI_VENDOR_ID as u32);
    // Transitional device id assigned to virtio-blk.
    assert_eq!((id >> 16) & 0xFFFF, 0x1001);
}

#[test]
fn pci_identity_net() {
    let dev = make_device_of_type(bits::VIRTIO_DEV_TYPE_NET);
    let id = dev.cfg_read(0x00, 4);
    assert_eq!((id >> 16) & 0xFFFF, 0x1000);
}

#[test]
fn pci_identity_rng() {
    let dev = make_device_of_type(bits::VIRTIO_DEV_TYPE_RNG);
    let id = dev.cfg_read(0x00, 4);
    // Regression: the old formula advertised 0x1003, which belongs to
    // virtio-console.
    assert_eq!((id >> 16) & 0xFFFF, 0x1005);
}

#[test]
fn pci_identity_console() {
    let dev = make_device_of_type(bits::VIRTIO_DEV_TYPE_CONSOLE);
    let id = dev.cfg_read(0x00, 4);
    assert_eq!((id >> 16) & 0xFFFF, 0x1003);
    let class_reg = dev.cfg_read(0x08, 4);
    assert_eq!(
        (class_reg >> 24) & 0xFF,
        vmm_devices::pci::bits::CLASS_COMMUNICATION as u32
    );
    assert_eq!(
        (class_reg >> 16) & 0xFF,
        vmm_devices::pci::bits::SUBCLASS_COMMUNICATION_OTHER as u32
    );
}

#[test]
fn pci_class() {
    let dev = make_test_device();
    let class_reg = dev.cfg_read(0x08, 4);
    let class = (class_reg >> 24) & 0xFF;
    assert_eq!(class, vmm_devices::pci::bits::CLASS_STORAGE as u32);
}

/// Build a device with no device-specific config, as virtio-rnd has.
fn make_device_without_config() -> Arc<VirtioPciDevice<TestVirtioDevice>> {
    let physmap = Arc::new(PhysMap::new());
    VirtioPciDevice::new(
        TestVirtioDevice { features: 0 },
        bits::VIRTIO_DEV_TYPE_RNG,
        1,
        64,
        0, // no device config, like virtio-rnd
        None,
        physmap,
        Arc::new(PioBus::new()),
        Arc::new(MmioBus::new()),
        None,
    )
}

/// Walk the capability chain, returning (cfg_type, length) per virtio
/// capability in link order.
fn walk_virtio_caps<D: VirtioDevice>(
    dev: &VirtioPciDevice<D>,
) -> Vec<(u8, u32)> {
    let mut out = Vec::new();
    // Capabilities pointer at 0x34, low byte.
    let mut pos = (dev.cfg_read(0x34, 4) & 0xFF) as u8;
    // Bounded: a malformed chain must not spin the test forever.
    for _ in 0..16 {
        if pos == 0 {
            break;
        }
        let hdr = dev.cfg_read(pos, 4);
        let cap_id = (hdr & 0xFF) as u8;
        let next = ((hdr >> 8) & 0xFF) as u8;
        if cap_id == bits::PCI_CAP_ID_VNDR {
            let cfg_type = ((hdr >> 24) & 0xFF) as u8;
            let length = dev.cfg_read(pos + 12, 4);
            out.push((cfg_type, length));
        }
        pos = next;
    }
    out
}

/// A capability advertising length 0 is not ignored by Linux: its
/// `map_capability()` rejects `length <= start` and fails the whole
/// probe, so the device never binds. VIRTIO 1.3 4.1.4.6 makes the
/// device-config capability conditional on having such config.
#[test]
fn device_without_config_omits_the_device_cfg_cap() {
    let dev = make_device_without_config();
    let caps = walk_virtio_caps(&dev);

    assert!(
        !caps
            .iter()
            .any(|(t, _)| *t == bits::VIRTIO_PCI_CAP_DEVICE_CFG),
        "device with no config still advertises a device-cfg cap: {caps:?}"
    );
    // Whatever is advertised must be usable.
    for (cfg_type, length) in &caps {
        assert_ne!(*length, 0, "cap type {cfg_type} has length 0");
    }
    // The chain must still reach the caps a driver needs.
    for want in [
        bits::VIRTIO_PCI_CAP_COMMON_CFG,
        bits::VIRTIO_PCI_CAP_NOTIFY_CFG,
        bits::VIRTIO_PCI_CAP_ISR_CFG,
    ] {
        assert!(
            caps.iter().any(|(t, _)| *t == want),
            "cap type {want} missing from {caps:?}"
        );
    }
}

#[test]
fn device_with_config_keeps_the_device_cfg_cap() {
    let dev = make_test_device();
    let caps = walk_virtio_caps(&dev);
    let found = caps
        .iter()
        .find(|(t, _)| *t == bits::VIRTIO_PCI_CAP_DEVICE_CFG)
        .expect("device-cfg cap present");
    assert_eq!(
        found.1, 8,
        "device-cfg cap length should be the config size"
    );
}

#[test]
fn pci_subsystem() {
    let dev = make_test_device();
    let sub = dev.cfg_read(0x2C, 4);
    assert_eq!(sub & 0xFFFF, bits::VIRTIO_PCI_VENDOR_ID as u32);
    assert_eq!((sub >> 16) & 0xFFFF, bits::VIRTIO_DEV_TYPE_BLOCK as u32);
}

#[test]
fn bar0_is_pio() {
    let dev = make_test_device();
    // Write all-ones to BAR0 to probe
    dev.cfg_write(0x10, 4, 0xFFFF_FFFF);
    let bar = dev.cfg_read(0x10, 4);
    // Low bit should be 1 (PIO)
    assert_eq!(bar & 1, 1);
}

#[test]
fn device_features_read() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x00, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    // Device features = 0x42 from test device (EVENT_IDX disabled pending investigation)
    assert_eq!(val, 0x42);
}

#[test]
fn device_status_readwrite() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Write status
    let wo = WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    // Read status back
    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], bits::STATUS_ACKNOWLEDGE);
}

#[test]
fn device_status_zero_resets() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Set some status
    let wo =
        WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    // Reset by writing 0
    let wo = WriteOp::from_buf(&[0]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    // Status should be 0
    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], 0);
}

#[test]
fn isr_read_clears() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // Manually set ISR
    dev.isr_status
        .store(bits::ISR_QUEUE_INTR, std::sync::atomic::Ordering::Relaxed);

    // First read should return the value
    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x13, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], bits::ISR_QUEUE_INTR);

    // Second read should return 0 (cleared)
    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x13, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], 0);
}

#[test]
fn queue_select_and_size() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Select queue 0
    let wo = WriteOp::from_buf(&[0, 0]);
    dev.bar_rw(BarN::BAR0, 0x0E, RWOp::Write(&wo));

    // Read queue size
    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR0, 0x0C, RWOp::Read(&mut ro));
    let size = u16::from_le_bytes([ro.buf()[0], ro.buf()[1]]);
    assert_eq!(size, 256);

    // Select invalid queue
    let wo = WriteOp::from_buf(&[1, 0]);
    dev.bar_rw(BarN::BAR0, 0x0E, RWOp::Write(&wo));

    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR0, 0x0C, RWOp::Read(&mut ro));
    let size = u16::from_le_bytes([ro.buf()[0], ro.buf()[1]]);
    assert_eq!(size, 0);
}

#[test]
fn device_config_read() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // Read device-specific config at offset 0x14 (dev offset 0)
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x14, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    // TestVirtioDevice returns offset as value
    assert_eq!(val, 0);

    // At offset 0x18 (dev offset 4)
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x18, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(val, 4);
}

#[test]
fn guest_features_write_read() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Device offers 0x42; write 0x42 so masking yields 0x42.
    let wo = WriteOp::from_buf(&[0x42, 0x00, 0x00, 0x00]);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Write(&wo));

    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(val, 0x42);
}

#[test]
fn guest_features_masked_to_offered() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Write 0xFFFFFFFF — should be masked to offered features (0x42)
    let wo = WriteOp::from_buf(&[0xFF, 0xFF, 0xFF, 0xFF]);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Write(&wo));

    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(val, 0x42, "guest features should be masked to offered 0x42");
}

#[test]
fn guest_features_write_unsupported_is_rejected() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Write a feature not offered (0x100)
    let wo = WriteOp::from_buf(&[0x00, 0x01, 0x00, 0x00]);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Write(&wo));

    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    // Only offered features survive masking
    assert_eq!(val, 0x00);
}

#[test]
fn device_config_offset_without_msix() {
    let dev = make_test_device();
    // Without MSI-X, device config starts at 0x14
    assert_eq!(dev.device_config_offset(), bits::LEGACY_REG_DEVICE_CONFIG);
    assert_eq!(dev.device_config_offset(), 0x14);
}

#[test]
fn device_config_offset_msix_constant() {
    // Without a VmmHdl we cannot construct MsixTable, but we can
    // verify the constants that determine the offset.
    assert_eq!(bits::LEGACY_REG_DEVICE_CONFIG, 0x14);
    assert_eq!(bits::LEGACY_REG_DEVICE_CONFIG_MSIX, 0x18);
    // The difference is 4 bytes (two 16-bit vector registers)
    assert_eq!(
        bits::LEGACY_REG_DEVICE_CONFIG_MSIX - bits::LEGACY_REG_DEVICE_CONFIG,
        4,
    );
}

#[test]
fn queue_select_invalid_returns_zero_size() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Device has 1 queue (index 0). Select queue 3 (invalid).
    let wo = WriteOp::from_buf(&[3, 0]);
    dev.bar_rw(BarN::BAR0, 0x0E, RWOp::Write(&wo));

    // Queue size for invalid index should be 0
    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR0, 0x0C, RWOp::Read(&mut ro));
    let size = u16::from_le_bytes([ro.buf()[0], ro.buf()[1]]);
    assert_eq!(size, 0, "invalid queue index should report size 0");

    // Queue PFN for invalid index should also be 0
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x08, RWOp::Read(&mut ro));
    let pfn = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(pfn, 0, "invalid queue index should report PFN 0");
}

#[test]
fn features_ok_set_and_readable() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Walk through the status progression: ACK -> DRIVER -> FEATURES_OK
    let wo = WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    let wo =
        WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    let wo = WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE
        | bits::STATUS_DRIVER
        | bits::STATUS_FEATURES_OK]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    // Read back — FEATURES_OK should be set
    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Read(&mut ro));
    assert_ne!(
        ro.buf()[0] & bits::STATUS_FEATURES_OK,
        0,
        "FEATURES_OK should be set after valid feature negotiation"
    );
}

#[test]
fn queue_notify_ignored_before_driver_ok() {
    use vmm_core::common::WriteOp;
    let dev = make_test_device();

    // Don't set DRIVER_OK status. Notify queue 0 — should be a no-op.
    // (Verifying no panic or crash)
    let wo = WriteOp::from_buf(&[0, 0]);
    dev.bar_rw(BarN::BAR0, 0x10, RWOp::Write(&wo));
}

#[test]
fn isr_cfg_change_bit() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // Set config change ISR bit
    dev.isr_status
        .store(bits::ISR_CFG_CHANGE, std::sync::atomic::Ordering::Relaxed);

    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x13, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], bits::ISR_CFG_CHANGE);

    // Should be cleared after read
    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x13, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], 0);
}

#[test]
fn isr_combined_bits() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // Set both ISR bits
    dev.isr_status.store(
        bits::ISR_QUEUE_INTR | bits::ISR_CFG_CHANGE,
        std::sync::atomic::Ordering::Relaxed,
    );

    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x13, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], bits::ISR_QUEUE_INTR | bits::ISR_CFG_CHANGE,);
}

#[test]
fn device_features_write_ignored() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Device features register (0x00) is read-only
    let wo = WriteOp::from_buf(&[0xFF, 0xFF, 0xFF, 0xFF]);
    dev.bar_rw(BarN::BAR0, 0x00, RWOp::Write(&wo));

    // Should still read as original device features
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x00, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(val, 0x42, "device features should be read-only");
}

// -- Modern transport tests --

#[test]
fn bar2_is_mmio() {
    let dev = make_test_device();
    // Write all-ones to BAR2 to probe
    dev.cfg_write(0x18, 4, 0xFFFF_FFFF);
    let bar = dev.cfg_read(0x18, 4);
    // Low bit 0 = MMIO (not PIO)
    assert_eq!(bar & 1, 0, "BAR2 should be MMIO");
    // Size mask for 16KB: ~(0x4000 - 1) = 0xFFFF_C000
    assert_eq!(bar & 0xFFFF_C000, 0xFFFF_C000, "BAR2 size should be 16KB");
}

#[test]
fn export_virtio_state_includes_pci_transport_state() {
    let dev = make_test_device();
    {
        let mut vs = dev.virtio_state.lock().expect("virtio lock");
        vs.queues[0].set_addr_modern(0x1000, 0x2000, 0x3000);
        vs.queue_enabled[0] = true;
    }

    let command = vmm_devices::pci::bits::RegCmd::IO_EN
        | vmm_devices::pci::bits::RegCmd::MMIO_EN
        | vmm_devices::pci::bits::RegCmd::BUSMSTR_EN
        | vmm_devices::pci::bits::RegCmd::INTX_DIS;
    dev.cfg_write(vmm_devices::pci::bits::REG_BAR0, 4, 0xC120);
    dev.cfg_write(vmm_devices::pci::bits::REG_BAR2, 4, 0xFEC0_0000);
    dev.cfg_write(
        vmm_devices::pci::bits::REG_COMMAND,
        2,
        u32::from(command.bits()),
    );

    let exported = dev.export_virtio_state();
    assert_eq!(exported.len(), 1);

    let pci_state = exported[0]
        .pci_state
        .as_ref()
        .expect("pci transport state should be exported");
    assert_eq!(pci_state.command, command.bits());
    assert_eq!(pci_state.bar_addrs[BarN::BAR0 as usize], 0xC120);
    assert_eq!(pci_state.bar_addrs[BarN::BAR2 as usize], 0xFEC0_0000);
    assert_eq!(pci_state.bar_addrs[BarN::BAR4 as usize], 0);
}

#[test]
fn restore_virtio_state_replays_pci_transport_state() {
    let dev = make_tracking_device();
    let command = vmm_devices::pci::bits::RegCmd::IO_EN
        | vmm_devices::pci::bits::RegCmd::MMIO_EN
        | vmm_devices::pci::bits::RegCmd::BUSMSTR_EN
        | vmm_devices::pci::bits::RegCmd::INTX_DIS;
    let queue_state = vmm_devices::lifecycle::VirtioMigrateQueue {
        queue_idx: 0,
        queue_size: 256,
        desc_addr: 0x1000,
        avail_addr: 0x2000,
        used_addr: 0x3000,
        enabled: true,
        features: bits::VIRTIO_F_VERSION_1,
        avail_idx: 7,
        used_idx: 5,
        msix_addr: 0,
        msix_data: 0,
        msix_vector: bits::VIRTIO_MSI_NO_VECTOR,
        pci_state: Some(vmm_devices::lifecycle::VirtioMigratePciState {
            command: command.bits(),
            bar_addrs: [0xC120, 0, 0xFEC0_0000, 0, 0, 0],
        }),
    };

    dev.restore_virtio_state(&[queue_state]);

    assert_eq!(dev.cfg_read(vmm_devices::pci::bits::REG_BAR0, 4), 0xC121);
    assert_eq!(
        dev.cfg_read(vmm_devices::pci::bits::REG_BAR2, 4),
        0xFEC0_0000
    );
    assert_eq!(
        dev.cfg_read(vmm_devices::pci::bits::REG_COMMAND, 2),
        u32::from(command.bits()),
    );

    let tracked = dev.device.state.lock().expect("tracking state lock");
    assert_eq!(tracked.reset_all_rings, 1);
    assert_eq!(tracked.set_features, vec![bits::VIRTIO_F_VERSION_1]);
    assert_eq!(
        tracked.restored_rings,
        vec![RestoreRingCall {
            queue_idx: 0,
            size: 256,
            desc: 0x1000,
            avail: 0x2000,
            used: 0x3000,
            avail_idx: 7,
            used_idx: 5,
            msix_addr: 0,
            msix_data: 0,
        }],
    );
    assert_eq!(
        tracked.notify_addrs,
        vec![(
            0xC120 + bits::LEGACY_REG_QUEUE_NOTIFY,
            0xFEC0_0000 + bits::MODERN_BAR_NOTIFY_OFFSET as u64,
        )],
    );
}

#[test]
fn restore_virtio_state_propagates_ring_feature_flags() {
    let dev = make_tracking_device();
    let features = bits::VIRTIO_F_VERSION_1
        | bits::VIRTIO_F_RING_EVENT_IDX
        | bits::VIRTIO_F_RING_INDIRECT_DESC;
    let queue_state = vmm_devices::lifecycle::VirtioMigrateQueue {
        queue_idx: 0,
        queue_size: 256,
        desc_addr: 0x1000,
        avail_addr: 0x2000,
        used_addr: 0x3000,
        enabled: true,
        features,
        avail_idx: 0,
        used_idx: 0,
        msix_addr: 0,
        msix_data: 0,
        msix_vector: bits::VIRTIO_MSI_NO_VECTOR,
        pci_state: None,
    };

    dev.restore_virtio_state(&[queue_state]);

    let vs = dev.virtio_state.lock().expect("lock");
    assert!(
        vs.queues[0].event_idx_enabled(),
        "event_idx must be propagated to queues during restore",
    );
    assert!(
        vs.queues[0].indirect_supported(),
        "indirect_supported must be propagated to queues during restore",
    );
}

#[test]
fn cap_chain_starts_at_0x40() {
    let dev = make_test_device();
    // Capabilities pointer at 0x34
    let cap_ptr = dev.cfg_read(0x34, 1);
    assert_eq!(cap_ptr, 0x40, "cap chain should start at 0x40");
}

#[test]
fn cap_chain_byte_reads() {
    let dev = make_test_device();
    // Byte read at 0x40 → cap_vndr = 0x09
    assert_eq!(dev.cfg_read(0x40, 1), 0x09, "byte read: cap_vndr");
    // Byte read at 0x41 → cap_next = 0x50 (notify cap)
    assert_eq!(dev.cfg_read(0x41, 1), 0x50, "byte read: cap_next");
    // Byte read at 0x42 → cap_len = 16
    assert_eq!(dev.cfg_read(0x42, 1), 16, "byte read: cap_len");
    // Byte read at 0x43 → cfg_type = 1 (common)
    assert_eq!(dev.cfg_read(0x43, 1), 1, "byte read: cfg_type");
}

#[test]
fn cap_chain_common_cfg() {
    let dev = make_test_device();
    // No MSI-X → common cap at 0x40
    let dword0 = dev.cfg_read(0x40, 4);
    // Byte 0: cap_vndr = 0x09
    assert_eq!(dword0 & 0xFF, 0x09, "cap_vndr should be 0x09");
    // Byte 3: cfg_type = 1 (common)
    assert_eq!((dword0 >> 24) & 0xFF, 1, "cfg_type should be COMMON_CFG");
    // Bar field at offset+4
    let dword1 = dev.cfg_read(0x44, 4);
    assert_eq!(
        dword1 & 0xFF,
        bits::MODERN_BAR_IDX as u32,
        "common cap should point to BAR2"
    );
    // Offset within BAR at offset+8
    let dword2 = dev.cfg_read(0x48, 4);
    assert_eq!(
        dword2,
        bits::MODERN_BAR_COMMON_OFFSET,
        "common cap offset should be 0"
    );
}

#[test]
fn cap_chain_notify_cfg() {
    let dev = make_test_device();
    // No MSI-X → notify cap at 0x50
    let dword0 = dev.cfg_read(0x50, 4);
    assert_eq!(dword0 & 0xFF, 0x09);
    assert_eq!((dword0 >> 24) & 0xFF, 2, "cfg_type should be NOTIFY_CFG");
    // Offset within BAR
    let offset = dev.cfg_read(0x58, 4);
    assert_eq!(offset, bits::MODERN_BAR_NOTIFY_OFFSET);
    // Multiplier (extra dword at cap+16)
    let mult = dev.cfg_read(0x60, 4);
    assert_eq!(mult, 0, "notify_off_multiplier should be 0");
}

#[test]
fn cap_chain_last_is_device_cfg() {
    let dev = make_test_device();
    // No MSI-X → device cap at 0x74
    let dword0 = dev.cfg_read(0x74, 4);
    assert_eq!(dword0 & 0xFF, 0x09);
    assert_eq!((dword0 >> 24) & 0xFF, 4, "cfg_type should be DEVICE_CFG");
    // Next pointer should be 0 (last in chain)
    assert_eq!((dword0 >> 8) & 0xFF, 0, "last cap next should be 0");
}

#[test]
fn modern_features_include_version_1() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Read modern features via BAR2: select high 32 bits
    let wo = WriteOp::from_buf(&[1, 0, 0, 0]);
    dev.bar_rw(BarN::BAR2, 0x00, RWOp::Write(&wo));

    // Read device_feature
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR2, 0x04, RWOp::Read(&mut ro));
    let high = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    // VERSION_1 = bit 32 → bit 0 of high word
    assert_ne!(high & 1, 0, "VERSION_1 should be in modern features");
}

#[test]
fn modern_features_include_event_idx() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // Read modern features: select low 32 bits (default)
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR2, 0x04, RWOp::Read(&mut ro));
    let low = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    // EVENT_IDX = bit 29
    assert_ne!(low & (1 << 29), 0, "EVENT_IDX should be in modern features");
    // INDIRECT_DESC = bit 28
    assert_ne!(
        low & (1 << 28),
        0,
        "INDIRECT_DESC should be in modern features"
    );
}

#[test]
fn legacy_features_no_event_idx() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // Legacy BAR0 device_features at offset 0x00
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x00, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    // Legacy should NOT include EVENT_IDX (bit 29)
    assert_eq!(
        val & (1 << 29),
        0,
        "EVENT_IDX must NOT appear in legacy features"
    );
}

#[test]
fn modern_num_queues() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // num_queues at common config offset 0x12
    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR2, 0x12, RWOp::Read(&mut ro));
    let nq = u16::from_le_bytes([ro.buf()[0], ro.buf()[1]]);
    assert_eq!(nq, 1, "should report 1 queue");
}

#[test]
fn modern_device_status() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Write status via BAR2
    let wo = WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE]);
    dev.bar_rw(BarN::BAR2, 0x14, RWOp::Write(&wo));

    // Read back
    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR2, 0x14, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], bits::STATUS_ACKNOWLEDGE);
}

// -- BAR teardown tests --

const TEST_BAR0_PORT: u16 = 0x2000;
const TEST_BAR2_ADDR: u64 = 0xE000_0000;

/// Build a device and keep handles to the buses it registers on.
fn make_device_with_buses() -> (
    Arc<VirtioPciDevice<TestVirtioDevice>>,
    Arc<PioBus>,
    Arc<MmioBus>,
) {
    let bus_pio = Arc::new(PioBus::new());
    let bus_mmio = Arc::new(MmioBus::new());
    let dev = VirtioPciDevice::new(
        TestVirtioDevice { features: 0 },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        Arc::new(PhysMap::new()),
        Arc::clone(&bus_pio),
        Arc::clone(&bus_mmio),
        None,
    );
    (dev, bus_pio, bus_mmio)
}

/// Program BAR0 and BAR2, then enable IO and MMIO decoding.
fn enable_bars(dev: &Arc<VirtioPciDevice<TestVirtioDevice>>) {
    dev.cfg_write(0x10, 4, u32::from(TEST_BAR0_PORT));
    dev.cfg_write(0x18, 4, TEST_BAR2_ADDR as u32);
    let cmd = vmm_devices::pci::bits::RegCmd::IO_EN
        | vmm_devices::pci::bits::RegCmd::MMIO_EN;
    dev.cfg_write(0x04, 2, u32::from(cmd.bits()));
    assert!(dev.registered_bar.lock().expect("bar lock").is_some());
    assert!(dev.registered_bar2.lock().expect("bar2 lock").is_some());
}

/// Every live BAR handler holds a strong self reference, so the
/// device cannot drop while one stays registered.
#[test]
fn bar_registration_holds_a_strong_self_reference() {
    let (dev, _bus_pio, _bus_mmio) = make_device_with_buses();
    assert_eq!(Arc::strong_count(&dev), 1);
    enable_bars(&dev);
    // One handler for BAR0 (PIO) and one for BAR2 (MMIO).
    assert_eq!(Arc::strong_count(&dev), 3);
}

/// A backend whose reset blocks until the test releases it, standing
/// in for one that must drain its I/O workers before it returns.
struct BlockingResetDevice {
    entered: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl VirtioDevice for BlockingResetDevice {
    fn device_features(&self) -> u64 {
        0
    }

    fn set_features(&self, _features: u64) {}

    fn cfg_read(&self, _offset: u16, _len: u8) -> u32 {
        0
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        0
    }

    fn reset(&self) {
        // A closed channel means the test already finished.
        let _ = self.entered.send(());
        let _ = self
            .release
            .lock()
            .expect("release lock")
            .recv_timeout(std::time::Duration::from_secs(10));
    }
}

impl Lifecycle for BlockingResetDevice {
    fn type_name(&self) -> &'static str {
        "blocking-reset"
    }
}

// A backend reset must drain its I/O workers before it returns, and
// the driver polls DEVICE_STATUS until it reads 0 (VirtIO 1.3 sec
// 4.1.4.3.1). Holding the transport lock across that wait stalls
// every other vCPU that touches this device.
#[test]
fn a_slow_backend_reset_does_not_hold_the_transport_lock() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use vmm_core::common::{ReadOp, WriteOp};

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let dev = VirtioPciDevice::new(
        BlockingResetDevice {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        Arc::new(PhysMap::new()),
        Arc::new(PioBus::new()),
        Arc::new(MmioBus::new()),
        None,
    );

    let writer = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || {
            let wo = WriteOp::from_buf(&[0]);
            dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));
        })
    };
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the backend reset ran");

    let reader = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || {
            let mut ro = ReadOp::new(1);
            dev.bar_rw(BarN::BAR0, 0x12, RWOp::Read(&mut ro));
        })
    };
    let deadline = Instant::now() + Duration::from_millis(500);
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let served = reader.is_finished();

    release_tx.send(()).expect("release the reset");
    writer.join().expect("status write");
    reader.join().expect("status read");
    assert!(served, "a register read blocked behind the backend reset");
}

#[test]
fn detach_regions_lets_the_device_drop() {
    let (dev, bus_pio, bus_mmio) = make_device_with_buses();
    enable_bars(&dev);

    let weak = Arc::downgrade(&dev);
    dev.detach_regions();
    // A second call must not unregister a base twice.
    dev.detach_regions();
    assert_eq!(Arc::strong_count(&dev), 1);

    drop(dev);
    assert!(weak.upgrade().is_none(), "device outlived its last handle");

    // The buses must no longer decode the old BAR ranges.
    assert_eq!(bus_pio.handle_in(TEST_BAR0_PORT, 1), 0xFF);
    assert_eq!(bus_mmio.handle_read(TEST_BAR2_ADDR, 4), u64::from(u32::MAX));
}
