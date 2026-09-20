// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the modern (VirtIO 1.0) half of the transport: the
//! capability chain, the common config registers and the feature bits.

use super::*;
use vmm_devices::DeviceMigrateState;

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

    let Some(DeviceMigrateState::VirtioQueues(exported)) =
        dev.export_migrate_state()
    else {
        panic!("a virtio transport exports its queues");
    };
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
        pci_slot: 0,
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

    dev.restore_migrate_state(&DeviceMigrateState::VirtioQueues(vec![
        queue_state,
    ]));

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
        pci_slot: 0,
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

    dev.restore_migrate_state(&DeviceMigrateState::VirtioQueues(vec![
        queue_state,
    ]));

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

fn read_modern_u16(dev: &VirtioPciDevice<TestVirtioDevice>, off: u16) -> u16 {
    use vmm_core::common::ReadOp;
    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR2, usize::from(off), RWOp::Read(&mut ro));
    u16::from_le_bytes([ro.buf()[0], ro.buf()[1]])
}

fn write_modern_u16(
    dev: &VirtioPciDevice<TestVirtioDevice>,
    off: u16,
    val: u16,
) {
    use vmm_core::common::WriteOp;
    let wo = WriteOp::from_buf(&val.to_le_bytes());
    dev.bar_rw(BarN::BAR2, usize::from(off), RWOp::Write(&wo));
}

fn modern_status(dev: &VirtioPciDevice<TestVirtioDevice>) -> u8 {
    read_modern_u16(dev, bits::COMMON_CFG_DEVICE_STATUS) as u8
}

/// A driver may shrink a ring before it enables it (VirtIO 1.3 sec
/// 4.1.4.3.2), and the device must then walk only that many entries.
/// A size it did not offer, or that is not a power of two, is refused
/// with DEVICE_NEEDS_RESET rather than dropped in silence.
#[test]
fn a_modern_driver_can_shrink_the_ring_and_a_bad_size_is_refused() {
    let dev = make_test_device();
    assert_eq!(read_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE), 256);

    write_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE, 64);
    assert_eq!(read_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE), 64);
    assert_eq!(
        dev.virtio_state.lock().expect("virtio lock").queues[0].size(),
        64,
        "the device kept its own size for the ring walk",
    );
    assert_eq!(modern_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET, 0);

    for bad in [0u16, 48, 512] {
        write_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE, bad);
        assert_eq!(
            read_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE),
            64,
            "size {bad} was taken",
        );
    }
    assert_ne!(modern_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET, 0);

    // A reset gives the offered size back.
    write_modern_u16(&dev, bits::COMMON_CFG_DEVICE_STATUS, 0);
    assert_eq!(read_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE), 256);
}
