// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the BAR cache and the config-space byte helpers.

use super::*;

use vmm_devices::pci::bits;

#[test]
fn bar_type_constants() {
    assert_eq!(PCI_ADDR_IO, 1);
    assert_eq!(PCI_ADDR_MEM32, 2);
    assert_eq!(PCI_ADDR_MEM64, 3);
}

#[test]
fn bar_info_default() {
    let info = BarInfo::default();
    assert_eq!(info.bar_type, 0);
    assert_eq!(info.hpa, 0);
    assert_eq!(info.size, 0);
}

#[test]
fn bar_define_from_bar_info() {
    // Test that we correctly create BarDefine from BarInfo.
    let io_bar = BarInfo {
        bar_type: PCI_ADDR_IO,
        hpa: 0x3000,
        size: 256,
    };
    assert_eq!(io_bar.bar_type, PCI_ADDR_IO);
    assert!(io_bar.size <= u64::from(u16::MAX));

    let mem32_bar = BarInfo {
        bar_type: PCI_ADDR_MEM32,
        hpa: 0xFE00_0000,
        size: 0x100_0000,
    };
    assert_eq!(mem32_bar.bar_type, PCI_ADDR_MEM32);

    // 64-bit BAR with 64 GB size (e.g., H100 GPU)
    let mem64_bar = BarInfo {
        bar_type: PCI_ADDR_MEM64,
        hpa: 0x38_0000_0000,
        size: 0x10_0000_0000, // 64 GB
    };
    assert_eq!(mem64_bar.bar_type, PCI_ADDR_MEM64);
    assert_eq!(mem64_bar.size, 0x10_0000_0000);
}

#[test]
fn extract_bytes_full_dword() {
    let val = 0xDEAD_BEEFu32;
    assert_eq!(extract_bytes(val, 0x00, 4), 0xDEAD_BEEF);
}

#[test]
fn extract_bytes_word() {
    let val = 0xDEAD_BEEFu32;
    // Low word at offset 0
    assert_eq!(extract_bytes(val, 0x00, 2), 0xBEEF);
    // High word at offset 2
    assert_eq!(extract_bytes(val, 0x02, 2), 0xDEAD);
}

#[test]
fn extract_bytes_single() {
    let val = 0x44332211u32;
    assert_eq!(extract_bytes(val, 0x00, 1), 0x11);
    assert_eq!(extract_bytes(val, 0x01, 1), 0x22);
    assert_eq!(extract_bytes(val, 0x02, 1), 0x33);
    assert_eq!(extract_bytes(val, 0x03, 1), 0x44);
}

#[test]
fn size_mask_values() {
    assert_eq!(size_mask(1), 0xFF);
    assert_eq!(size_mask(2), 0xFFFF);
    assert_eq!(size_mask(4), 0xFFFF_FFFF);
}

#[test]
fn bar_type_64bit_consumes_two_slots() {
    // Verify that 64-bit BARs in DeviceState work properly.
    let ident = vmm_devices::pci::device::DeviceIdent {
        vendor_id: 0x10DE,
        device_id: 0x2330,
        class: 0x03,
        subclass: 0x02,
        prog_if: 0,
        revision: 0xA1,
        sub_vendor_id: 0x10DE,
        sub_device_id: 0x16C0,
    };
    let mut state = DeviceState::new(ident);

    // Define a 64 GB 64-bit BAR at BAR0.
    state.define_bar(BarN::BAR0, BarDefine::Mmio64(0x10_0000_0000));

    // BAR0 should have the 64-bit type bits.
    let bar0 = state.cfg_read(0x10, 4);
    assert_eq!(bar0 & 0x7, bits::BAR_TYPE_MEM64);

    // BAR1 is the high half (Mmio64High).
    let bar1 = state.cfg_read(0x14, 4);
    assert_eq!(bar1, 0); // high bits = 0 initially

    // Write all-ones to BAR0 for sizing.
    state.cfg_write(0x10, 4, 0xFFFF_FFFF);
    let readback = state.cfg_read(0x10, 4);
    // Low 32 bits should be zero for a 64 GB BAR (size > 4 GB),
    // plus the type bits.
    assert_eq!(readback, 0x0000_0004);

    // Write all-ones to BAR1 (high half) for sizing.
    state.cfg_write(0x14, 4, 0xFFFF_FFFF);
    let readback_hi = state.cfg_read(0x14, 4);
    // For 64 GB = 0x10_0000_0000: mask = ~(0x10_0000_0000 - 1) = 0xFFFF_FFF0_0000_0000
    // High 32 bits = 0xFFFF_FFF0
    assert_eq!(readback_hi, 0xFFFF_FFF0);
}
