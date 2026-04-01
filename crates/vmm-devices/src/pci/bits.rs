// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI configuration space constants.
//!
//! Standard register offsets, bitfield definitions, class codes, and
//! port addresses used by PCI and PCIe configuration logic.

use bitflags::bitflags;

// ---------------------------------------------------------------------------
// Legacy PIO config space ports
// ---------------------------------------------------------------------------

/// Port address for the PCI configuration address register.
pub const PORT_PCI_CONFIG_ADDR: u16 = 0xCF8;

/// Length (in bytes) of the PCI configuration address register.
pub const LEN_PCI_CONFIG_ADDR: u16 = 4;

/// Port address for the PCI configuration data register.
pub const PORT_PCI_CONFIG_DATA: u16 = 0xCFC;

/// Length (in bytes) of the PCI configuration data register.
pub const LEN_PCI_CONFIG_DATA: u16 = 4;

// ---------------------------------------------------------------------------
// Config space sizes
// ---------------------------------------------------------------------------

/// Size of PCI (non-PCIe) configuration space per function.
pub const LEN_CFG: usize = 0x100;

/// Size of the standard (Type 0) header region within config space.
pub const LEN_CFG_STD: usize = 0x40;

/// Size of PCIe extended configuration space per function.
pub const LEN_CFG_ECAM: usize = 0x1000;

// ---------------------------------------------------------------------------
// Standard header register offsets (Type 0)
// ---------------------------------------------------------------------------

pub const REG_VENDOR_ID: u8 = 0x00;
pub const REG_DEVICE_ID: u8 = 0x02;
pub const REG_COMMAND: u8 = 0x04;
pub const REG_STATUS: u8 = 0x06;
pub const REG_REVISION_ID: u8 = 0x08;
pub const REG_PROG_IF: u8 = 0x09;
pub const REG_SUBCLASS: u8 = 0x0A;
pub const REG_CLASS: u8 = 0x0B;
pub const REG_CACHE_LINE_SIZE: u8 = 0x0C;
pub const REG_LATENCY_TIMER: u8 = 0x0D;
pub const REG_HEADER_TYPE: u8 = 0x0E;
pub const REG_BIST: u8 = 0x0F;
pub const REG_BAR0: u8 = 0x10;
pub const REG_BAR1: u8 = 0x14;
pub const REG_BAR2: u8 = 0x18;
pub const REG_BAR3: u8 = 0x1C;
pub const REG_BAR4: u8 = 0x20;
pub const REG_BAR5: u8 = 0x24;
pub const REG_CARDBUS_CIS_PTR: u8 = 0x28;
pub const REG_SUB_VENDOR_ID: u8 = 0x2C;
pub const REG_SUB_DEVICE_ID: u8 = 0x2E;
pub const REG_EXPANSION_ROM: u8 = 0x30;
pub const REG_CAP_PTR: u8 = 0x34;
pub const REG_INTR_LINE: u8 = 0x3C;
pub const REG_INTR_PIN: u8 = 0x3D;
pub const REG_MIN_GRANT: u8 = 0x3E;
pub const REG_MAX_LATENCY: u8 = 0x3F;

// ---------------------------------------------------------------------------
// Command register (offset 0x04) bitflags
// ---------------------------------------------------------------------------

bitflags! {
    /// PCI Command register bits.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RegCmd: u16 {
        /// Enable response to I/O space accesses.
        const IO_EN = 1 << 0;
        /// Enable response to memory space accesses.
        const MMIO_EN = 1 << 1;
        /// Enable bus mastering.
        const BUSMSTR_EN = 1 << 2;
        /// Disable legacy INTx interrupt generation.
        const INTX_DIS = 1 << 10;
    }
}

impl RegCmd {
    /// Reset to default state (INTx disabled).
    pub fn reset(&mut self) {
        *self = RegCmd::default();
    }
}

impl Default for RegCmd {
    fn default() -> Self {
        RegCmd::INTX_DIS
    }
}

// ---------------------------------------------------------------------------
// Status register (offset 0x06) bitflags
// ---------------------------------------------------------------------------

bitflags! {
    /// PCI Status register bits.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct RegStatus: u16 {
        /// Interrupt status (read-only, reflects INTx line state).
        const INTR_STATUS = 1 << 3;
        /// Capabilities list present.
        const CAP_LIST = 1 << 4;
    }
}

// ---------------------------------------------------------------------------
// BAR type encoding bits (low bits of BAR register value)
// ---------------------------------------------------------------------------

/// BAR type bit indicating I/O space.
pub const BAR_TYPE_IO: u32 = 0b01;

/// BAR type bits indicating 32-bit memory space.
pub const BAR_TYPE_MEM: u32 = 0b000;

/// BAR type bits indicating 64-bit memory space.
pub const BAR_TYPE_MEM64: u32 = 0b100;

// ---------------------------------------------------------------------------
// Header type field values
// ---------------------------------------------------------------------------

/// Standard (Type 0) device header.
pub const HEADER_TYPE_DEVICE: u8 = 0b0;

/// PCI-to-PCI bridge (Type 1) header.
pub const HEADER_TYPE_BRIDGE: u8 = 0b1;

/// Multi-function device flag (OR'd into header type).
pub const HEADER_TYPE_MULTIFUNC: u8 = 0b1000_0000;

// ---------------------------------------------------------------------------
// Class codes
// ---------------------------------------------------------------------------

pub const CLASS_UNCLASSIFIED: u8 = 0;
pub const CLASS_STORAGE: u8 = 1;
pub const CLASS_NETWORK: u8 = 2;
pub const CLASS_DISPLAY: u8 = 3;
pub const CLASS_MULTIMEDIA: u8 = 4;
pub const CLASS_MEMORY: u8 = 5;
pub const CLASS_BRIDGE: u8 = 6;

// Sub-classes under CLASS_STORAGE
pub const SUBCLASS_STORAGE_NVM: u8 = 8;

// Sub-classes under CLASS_BRIDGE
pub const SUBCLASS_BRIDGE_HOST: u8 = 0;
pub const SUBCLASS_BRIDGE_ISA: u8 = 1;
pub const SUBCLASS_BRIDGE_PCI: u8 = 4;
pub const SUBCLASS_BRIDGE_OTHER: u8 = 0x80;

// Programming interfaces
pub const PROGIF_ENTERPRISE_NVME: u8 = 2;

// ---------------------------------------------------------------------------
// Capability IDs
// ---------------------------------------------------------------------------

pub const CAP_ID_MSI: u8 = 0x05;
pub const CAP_ID_VENDOR: u8 = 0x09;
pub const CAP_ID_MSIX: u8 = 0x11;

// ---------------------------------------------------------------------------
// BDF field masks
// ---------------------------------------------------------------------------

/// Maximum device number (5 bits: 0-31).
pub const MASK_DEV: u8 = 0x1F;

/// Maximum function number (3 bits: 0-7).
pub const MASK_FUNC: u8 = 0x07;

/// Maximum bus number (8 bits: 0-255).
pub const MASK_BUS: u8 = 0xFF;

// ---------------------------------------------------------------------------
// PCIe ECAM constants
// ---------------------------------------------------------------------------

/// Minimum number of buses addressable by a single ECAM region.
pub const PCIE_MIN_BUSES_PER_ECAM_REGION: u16 = 2;

/// Maximum number of buses addressable by a single ECAM region.
pub const PCIE_MAX_BUSES_PER_ECAM_REGION: u16 = 256;

/// Mask to extract config space offset from an ECAM MMIO address.
pub const MASK_ECAM_CFG_OFFSET: usize = 0xFFF;

// ---------------------------------------------------------------------------
// PCI-to-PCI bridge constants
// ---------------------------------------------------------------------------

/// Bridge class code.
pub const BRIDGE_PROG_CLASS: u8 = 0x06;

/// Bridge sub-class code (PCI-to-PCI).
pub const BRIDGE_PROG_SUBCLASS: u8 = 0x04;

/// Bridge programming interface.
pub const BRIDGE_PROG_IF: u8 = 0x00;

/// Bridge secondary status register initial value.
pub const BRIDGE_SECONDARY_STATUS: u16 = 0x0000;

/// Mask for reserved low bits of bridge memory base/limit registers.
pub const BRIDGE_MEMORY_REG_MASK: u16 = 0xFFF0;
