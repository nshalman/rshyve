// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI device trait and standard Type 0 config space handler.
//!
//! Provides [`PciDevice`] for device emulation and [`DeviceState`] for
//! managing the standard 64-byte PCI configuration header.

use vmm_core::common::RWOp;

use super::bar::{BarDefine, Bars};
use super::bits;
use super::BarN;

// ---------------------------------------------------------------------------
// PciDevice trait
// ---------------------------------------------------------------------------

/// Trait for emulated PCI devices.
///
/// Implementations provide config space access and BAR I/O handling.
/// The [`DeviceState`] helper handles standard header fields; devices
/// only need to add custom behavior on top.
pub trait PciDevice: Send + Sync + 'static {
    /// Read from PCI configuration space.
    ///
    /// `offset` is the byte offset (0x00..0xFF) and `len` is the access
    /// width (1, 2, or 4). Returns the value in the low bits of the u32.
    fn cfg_read(&self, offset: u8, len: u8) -> u32;

    /// Write to PCI configuration space.
    ///
    /// `offset` is the byte offset, `len` the width, `val` the value
    /// in the low bits.
    fn cfg_write(&self, offset: u8, len: u8, val: u32);

    /// Handle a BAR I/O read or write.
    ///
    /// `bar` identifies which BAR region, `offset` is the byte offset
    /// within the BAR, and `rwo` is the operation.
    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>);

    /// Unregister every bus region this device has registered.
    ///
    /// A BAR handler holds a strong reference to its device, and the
    /// device holds the bus. The device cannot drop while a handler
    /// stays registered. Call this before you release the last handle
    /// to the device, such as on hot-unplug or VM teardown.
    ///
    /// Each implementation must clear the recorded bases, so a second
    /// call does nothing.
    fn detach_regions(&self) {}
}

// ---------------------------------------------------------------------------
// DeviceIdent
// ---------------------------------------------------------------------------

/// Static identification fields for a PCI device.
#[derive(Debug, Clone, Default)]
pub struct DeviceIdent {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub sub_vendor_id: u16,
    pub sub_device_id: u16,
}

// ---------------------------------------------------------------------------
// DeviceState
// ---------------------------------------------------------------------------

/// Manages the standard PCI Type 0 configuration header (64 bytes).
///
/// This struct tracks the mutable parts of the config header (command
/// register, BARs, interrupt line) and provides read/write methods
/// that enforce PCI semantics (read-only fields, BAR sizing protocol,
/// command register masking).
pub struct DeviceState {
    ident: DeviceIdent,
    command: bits::RegCmd,
    bars: Bars,
    intr_line: u8,
    intr_pin: u8,
    header_type: u8,
    /// Offset of the first capability in config space (0 = none).
    cap_ptr: u8,
}

impl DeviceState {
    /// Create a new device state with the given identity.
    ///
    /// The command register defaults to `INTX_DIS`, all BARs are
    /// undefined, and the header type is standard (Type 0).
    pub fn new(ident: DeviceIdent) -> Self {
        Self {
            ident,
            command: bits::RegCmd::default(),
            bars: Bars::new(),
            intr_line: 0xFF,
            intr_pin: 0,
            header_type: bits::HEADER_TYPE_DEVICE,
            cap_ptr: 0,
        }
    }

    /// Define a BAR for this device.
    pub fn define_bar(&mut self, bar: BarN, def: BarDefine) {
        self.bars.define(bar, def);
    }

    /// Set the interrupt pin (0 = none, 1 = INTA, 2 = INTB, etc.).
    pub fn set_intr_pin(&mut self, pin: u8) {
        self.intr_pin = pin;
    }

    /// Set the header type (e.g., multi-function flag).
    /// Set the PCI Interrupt Line register (0x3C).
    pub fn set_intr_line(&mut self, irq: u8) {
        self.intr_line = irq;
    }

    pub fn set_header_type(&mut self, htype: u8) {
        self.header_type = htype;
    }

    /// Set the capabilities pointer. This also sets the CAP_LIST
    /// bit in the status register on subsequent reads.
    pub fn set_cap_ptr(&mut self, offset: u8) {
        self.cap_ptr = offset;
    }

    /// Get a reference to the BAR state.
    pub fn bars(&self) -> &Bars {
        &self.bars
    }

    /// Get a mutable reference to the BAR state.
    pub fn bars_mut(&mut self) -> &mut Bars {
        &mut self.bars
    }

    /// Current command register value.
    pub fn command(&self) -> bits::RegCmd {
        self.command
    }

    /// Read from the standard PCI config header.
    ///
    /// Handles all standard Type 0 fields. Returns 0 for reserved or
    /// unimplemented fields.
    pub fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        // Only the standard header (0x00..0x3F) is handled here.
        // Offsets beyond the header return 0.
        if offset >= bits::LEN_CFG_STD as u8 {
            return 0;
        }

        // Align to dword boundary for the register lookup
        let dword_off = offset & 0xFC;
        let byte_off = (offset & 0x03) as u32;

        let dword_val = match dword_off {
            // 0x00: Vendor ID (low 16) | Device ID (high 16)
            0x00 => {
                u32::from(self.ident.vendor_id)
                    | (u32::from(self.ident.device_id) << 16)
            }
            // 0x04: Command (low 16) | Status (high 16)
            0x04 => {
                let mut status = bits::RegStatus::empty();
                if self.cap_ptr != 0 {
                    status |= bits::RegStatus::CAP_LIST;
                }
                u32::from(self.command.bits())
                    | (u32::from(status.bits()) << 16)
            }
            // 0x08: Revision (8) | Prog IF (8) | Subclass (8) | Class (8)
            0x08 => {
                u32::from(self.ident.revision)
                    | (u32::from(self.ident.prog_if) << 8)
                    | (u32::from(self.ident.subclass) << 16)
                    | (u32::from(self.ident.class) << 24)
            }
            // 0x0C: Cache Line Size | Latency Timer | Header Type | BIST
            0x0C => u32::from(self.header_type) << 16,
            // 0x10-0x24: BARs 0-5
            bar_off @ 0x10..=0x24 if (bar_off & 0x03) == 0 => {
                let bar_idx = (bar_off - 0x10) / 4;
                if let Ok(bar_n) = BarN::try_from(bar_idx) {
                    self.bars.reg_read(bar_n)
                } else {
                    0
                }
            }
            // 0x28: Cardbus CIS Pointer
            0x28 => 0,
            // 0x2C: Sub-Vendor ID (low 16) | Sub-Device ID (high 16)
            0x2C => {
                u32::from(self.ident.sub_vendor_id)
                    | (u32::from(self.ident.sub_device_id) << 16)
            }
            // 0x30: Expansion ROM Base Address
            0x30 => 0,
            // 0x34: Capabilities Pointer (low 8) | Reserved (24)
            0x34 => u32::from(self.cap_ptr),
            // 0x38: Reserved
            0x38 => 0,
            // 0x3C: Intr Line (8) | Intr Pin (8) | Min Grant (8) | Max Lat (8)
            0x3C => u32::from(self.intr_line) | (u32::from(self.intr_pin) << 8),
            _ => 0,
        };

        // Extract the requested bytes from the dword
        (dword_val >> (byte_off * 8)) & size_mask(len)
    }

    /// Write to the standard PCI config header.
    ///
    /// Only writable fields are modified; writes to read-only fields
    /// (vendor ID, device ID, class, etc.) are silently ignored.
    pub fn cfg_write(&mut self, offset: u8, len: u8, val: u32) {
        if offset >= bits::LEN_CFG_STD as u8 {
            return;
        }

        let dword_off = offset & 0xFC;
        let byte_off = (offset & 0x03) as u32;

        match dword_off {
            // 0x04: Command register (low 16 bits writable)
            0x04 if byte_off < 2 => {
                // Build a full 16-bit value considering byte offset
                let shift = byte_off * 8;
                let mask = size_mask(len) << shift;
                let old_bits = u32::from(self.command.bits());
                let new_bits = (old_bits & !mask) | ((val << shift) & mask);
                // Only the defined command bits are writable
                let writable = bits::RegCmd::IO_EN
                    | bits::RegCmd::MMIO_EN
                    | bits::RegCmd::BUSMSTR_EN
                    | bits::RegCmd::INTX_DIS;
                self.command = bits::RegCmd::from_bits_truncate(
                    (new_bits as u16) & writable.bits()
                        | self.command.bits() & !writable.bits(),
                );
                // Writes to the status register (offset 0x06-0x07) are
                // handled as write-1-to-clear but we have nothing clearable
                // right now, so they are ignored.
            }
            // 0x0C: Header Type (byte at offset 0x0E) — only the
            // multi-function bit (0x80) is writable by the bus layer.
            // Header type is byte 2 of this dword (offset 0x0E).
            0x0C if byte_off == 2 && len == 1 => {
                self.header_type = val as u8;
            }
            // 0x10-0x24: BAR writes
            bar_off @ 0x10..=0x24
                if (bar_off & 0x03) == 0 && byte_off == 0 && len == 4 =>
            {
                // BAR writes must be dword-aligned and full-width.
                let bar_idx = (bar_off - 0x10) / 4;
                if let Ok(bar_n) = BarN::try_from(bar_idx) {
                    self.bars.reg_write(bar_n, val);
                }
            }
            // 0x3C: Interrupt Line (byte at offset 0x3C is writable)
            0x3C if byte_off == 0 && len >= 1 => {
                self.intr_line = val as u8;
            }
            // All other fields: read-only, ignored
            _ => {}
        }
    }
}

/// Build a bitmask for the given access width.
fn size_mask(len: u8) -> u32 {
    match len {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => 0xFFFF_FFFF,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ident() -> DeviceIdent {
        DeviceIdent {
            vendor_id: 0x8086,
            device_id: 0x1237,
            class: bits::CLASS_BRIDGE,
            subclass: bits::SUBCLASS_BRIDGE_HOST,
            prog_if: 0,
            revision: 0x02,
            sub_vendor_id: 0xFB5D,
            sub_device_id: 0x0001,
        }
    }

    #[test]
    fn read_vendor_device_id() {
        let state = DeviceState::new(test_ident());
        // Full dword read at offset 0
        let val = state.cfg_read(0x00, 4);
        assert_eq!(val & 0xFFFF, 0x8086);
        assert_eq!((val >> 16) & 0xFFFF, 0x1237);
    }

    #[test]
    fn read_vendor_id_word() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x00, 2);
        assert_eq!(val, 0x8086);
    }

    #[test]
    fn read_device_id_word() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x02, 2);
        assert_eq!(val, 0x1237);
    }

    #[test]
    fn read_class_subclass() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x08, 4);
        // Revision at byte 0
        assert_eq!(val & 0xFF, 0x02);
        // Class at byte 3
        assert_eq!((val >> 24) & 0xFF, bits::CLASS_BRIDGE as u32);
        // Subclass at byte 2
        assert_eq!((val >> 16) & 0xFF, bits::SUBCLASS_BRIDGE_HOST as u32);
    }

    #[test]
    fn read_header_type() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x0E, 1);
        assert_eq!(val, bits::HEADER_TYPE_DEVICE as u32);
    }

    #[test]
    fn command_register_write_read() {
        let mut state = DeviceState::new(test_ident());
        // Write IO_EN | MMIO_EN
        state.cfg_write(0x04, 2, 0x0003);
        let val = state.cfg_read(0x04, 2);
        assert_eq!(val & 0x03, 0x03);
    }

    #[test]
    fn command_register_masks_reserved_bits() {
        let mut state = DeviceState::new(test_ident());
        // Write all bits set
        state.cfg_write(0x04, 2, 0xFFFF);
        let cmd = state.cfg_read(0x04, 2);
        // Only IO_EN, MMIO_EN, BUSMSTR_EN, INTX_DIS should be set
        let expected = bits::RegCmd::IO_EN
            | bits::RegCmd::MMIO_EN
            | bits::RegCmd::BUSMSTR_EN
            | bits::RegCmd::INTX_DIS;
        assert_eq!(cmd, expected.bits() as u32);
    }

    #[test]
    fn bar_sizing_protocol() {
        let mut state = DeviceState::new(test_ident());
        state.define_bar(BarN::BAR0, BarDefine::Mmio(0x1000));

        // Write all-ones to probe size
        state.cfg_write(0x10, 4, 0xFFFF_FFFF);
        let readback = state.cfg_read(0x10, 4);
        // Size is 0x1000, so mask = ~(0x1000 - 1) = 0xFFFF_F000
        // Type bits for 32-bit MMIO = 0b000
        assert_eq!(readback, 0xFFFF_F000);
    }

    #[test]
    fn bar_address_write() {
        let mut state = DeviceState::new(test_ident());
        state.define_bar(BarN::BAR0, BarDefine::Pio(0x100));

        state.cfg_write(0x10, 4, 0x3000);
        let val = state.cfg_read(0x10, 4);
        // PIO bar has low bit set
        assert_eq!(val, 0x3001);
    }

    #[test]
    fn intr_line_write_read() {
        let mut state = DeviceState::new(test_ident());
        state.cfg_write(0x3C, 1, 0x0A);
        let val = state.cfg_read(0x3C, 1);
        assert_eq!(val, 0x0A);
    }

    #[test]
    fn intr_pin_read_only() {
        let mut state = DeviceState::new(test_ident());
        state.set_intr_pin(1); // INTA
        let val = state.cfg_read(0x3D, 1);
        assert_eq!(val, 1);
    }

    #[test]
    fn sub_vendor_device_id() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x2C, 4);
        assert_eq!(val & 0xFFFF, 0xFB5D);
        assert_eq!((val >> 16) & 0xFFFF, 0x0001);
    }

    #[test]
    fn read_only_fields_not_writable() {
        let mut state = DeviceState::new(test_ident());
        // Try writing to vendor ID
        state.cfg_write(0x00, 2, 0xBEEF);
        assert_eq!(state.cfg_read(0x00, 2), 0x8086);
    }

    #[test]
    fn beyond_std_header_returns_zero() {
        let state = DeviceState::new(test_ident());
        assert_eq!(state.cfg_read(0x40, 4), 0);
        assert_eq!(state.cfg_read(0x80, 4), 0);
    }

    #[test]
    fn empty_bar_reads_zero() {
        let state = DeviceState::new(test_ident());
        assert_eq!(state.cfg_read(0x10, 4), 0);
        assert_eq!(state.cfg_read(0x14, 4), 0);
    }
}
