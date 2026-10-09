// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI device model types and utilities.
//!
//! Provides the core PCI abstractions used by device emulation:
//! - [`Bdf`] - Bus/Device/Function address
//! - [`BarN`] - BAR slot identifier
//! - [`PioCfgDecoder`] - Legacy 0xCF8/0xCFC config space access decoder
//! - BAR management via the [`bar`] module

use std::fmt;
use std::sync::Mutex;

use vmm_core::common::RWOp;

pub mod bar;
pub mod bits;
pub mod bus;
pub mod device;
pub mod msix;

pub use bar::{BarDefine, BarWriteResult, Bars};
pub use bus::PciBus;
pub use device::{DeviceIdent, DeviceState, PciDevice};

// ---------------------------------------------------------------------------
// Bdf - Bus/Device/Function
// ---------------------------------------------------------------------------

/// PCI Bus/Device/Function address.
///
/// Identifies a specific function within the PCI topology. Device numbers
/// range from 0 to 31 and function numbers from 0 to 7.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Bdf {
    bus: u8,
    dev: u8,
    func: u8,
}

impl Bdf {
    /// Create a new BDF, returning `None` if `dev` or `func` are out of range.
    pub const fn new(bus: u8, dev: u8, func: u8) -> Option<Self> {
        if dev > bits::MASK_DEV || func > bits::MASK_FUNC {
            None
        } else {
            Some(Self { bus, dev, func })
        }
    }

    /// Create a new BDF without checking field ranges.
    ///
    /// # Panics
    ///
    /// Panics if `dev > 31` or `func > 7`.
    pub const fn new_unchecked(bus: u8, dev: u8, func: u8) -> Self {
        assert!(dev <= bits::MASK_DEV, "device number exceeds max (31)");
        assert!(func <= bits::MASK_FUNC, "function number exceeds max (7)");
        Self { bus, dev, func }
    }

    /// Bus number (0-255).
    #[inline]
    pub const fn bus(&self) -> u8 {
        self.bus
    }

    /// Device number (0-31).
    #[inline]
    pub const fn dev(&self) -> u8 {
        self.dev
    }

    /// Function number (0-7).
    #[inline]
    pub const fn func(&self) -> u8 {
        self.func
    }
}

impl fmt::Debug for Bdf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bdf({}.{}.{})", self.bus, self.dev, self.func)
    }
}

impl fmt::Display for Bdf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.bus, self.dev, self.func)
    }
}

// ---------------------------------------------------------------------------
// BarN - BAR slot identifier
// ---------------------------------------------------------------------------

/// Identifies one of the six BAR slots in a PCI Type 0 header.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum BarN {
    BAR0 = 0,
    BAR1 = 1,
    BAR2 = 2,
    BAR3 = 3,
    BAR4 = 4,
    BAR5 = 5,
}

impl TryFrom<u8> for BarN {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(BarN::BAR0),
            1 => Ok(BarN::BAR1),
            2 => Ok(BarN::BAR2),
            3 => Ok(BarN::BAR3),
            4 => Ok(BarN::BAR4),
            5 => Ok(BarN::BAR5),
            _ => Err(()),
        }
    }
}

// ---------------------------------------------------------------------------
// INTxPinID - Legacy interrupt pin
// ---------------------------------------------------------------------------

/// PCI legacy interrupt pin identifier.
///
/// Values correspond to the INTx pin number as specified in the PCI
/// config space Interrupt Pin register (1 = INTA, 2 = INTB, etc.).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum INTxPinID {
    IntA = 1,
    IntB = 2,
    IntC = 3,
    IntD = 4,
}

// Re-export IntrPin from vmm-core (single source of truth).
pub use vmm_core::intr_pins::IntrPin;

/// Legacy interrupt configuration: a pin ID paired with the pin object.
pub type LintrCfg = (INTxPinID, std::sync::Arc<dyn IntrPin>);

// ---------------------------------------------------------------------------
// PioCfgDecoder - Legacy 0xCF8/0xCFC config access
// ---------------------------------------------------------------------------

/// Decodes legacy PCI configuration space accesses via ports 0xCF8/0xCFC.
///
/// The guest writes a 32-bit address to port 0xCF8 with the following format:
///
/// ```text
/// Bit 31      : Enable
/// Bits 23:16  : Bus number
/// Bits 15:11  : Device number
/// Bits 10:8   : Function number
/// Bits 7:2    : Register number (dword-aligned offset)
/// Bits 1:0    : (reserved, used as byte offset within dword)
/// ```
///
/// The guest then reads/writes 1-4 bytes at port 0xCFC + byte_offset.
pub struct PioCfgDecoder {
    addr: Mutex<u32>,
}

impl PioCfgDecoder {
    /// Create a new decoder with the address register cleared.
    pub fn new() -> Self {
        Self {
            addr: Mutex::new(0),
        }
    }

    /// Handle an access to the config address register (port 0xCF8).
    ///
    /// Only 4-byte aligned dword accesses are honored; sub-dword
    /// accesses to the address register are ignored per convention.
    pub fn service_addr(&self, port: u16, rwo: RWOp<'_>) {
        let offset = port - bits::PORT_PCI_CONFIG_ADDR;
        // Only handle aligned 4-byte access to the address register
        if offset != 0 || rwo.len() != 4 {
            return;
        }
        let mut addr = self.addr.lock().unwrap();
        match rwo {
            RWOp::Read(ro) => ro.write_u32(*addr),
            RWOp::Write(wo) => *addr = wo.read_u32(),
        }
    }

    /// Latch an address a migration carried.
    pub fn set_addr(&self, addr: u32) {
        *self.addr.lock().unwrap() = addr;
    }

    /// Handle an access to the config data register (port 0xCFC-0xCFF).
    ///
    /// Decodes the previously latched address and calls `cb` with the
    /// target BDF, the byte offset into config space, and the I/O
    /// operation. The callback should service the config space read or
    /// write.
    ///
    /// If the enable bit is not set, reads return all-ones and writes
    /// are silently dropped.
    pub fn service_data(
        &self,
        port: u16,
        rwo: RWOp<'_>,
        mut cb: impl FnMut(&Bdf, u8, RWOp<'_>),
    ) {
        let locked_addr = self.addr.lock().unwrap();
        let addr = *locked_addr;
        drop(locked_addr);

        // Byte offset within the dword comes from the data port number
        let byte_in_dword = (port - bits::PORT_PCI_CONFIG_DATA) as u8;

        if let Some((bdf, reg_off)) = cfg_addr_parse(addr) {
            let cfg_offset = reg_off + byte_in_dword;
            cb(&bdf, cfg_offset, rwo);
        } else {
            // Enable bit not set: reads get all-ones, writes are dropped
            if let RWOp::Read(ro) = rwo {
                match ro.len() {
                    1 => ro.write_u8(0xFF),
                    2 => ro.write_u16(0xFFFF),
                    4 => ro.write_u32(0xFFFF_FFFF),
                    _ => ro.write_u32(0xFFFF_FFFF),
                }
            }
        }
    }

    /// Read the current config address register value.
    pub fn addr(&self) -> u32 {
        *self.addr.lock().unwrap()
    }
}

impl Default for PioCfgDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for PioCfgDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PioCfgDecoder").finish_non_exhaustive()
    }
}

/// Parse a PCI config address register value into a BDF and register offset.
///
/// Returns `None` if the enable bit (bit 31) is not set.
fn cfg_addr_parse(addr: u32) -> Option<(Bdf, u8)> {
    if addr & 0x8000_0000 == 0 {
        // Enable bit not set
        return None;
    }

    let bus = (addr >> 16) as u8 & bits::MASK_BUS;
    let dev = (addr >> 11) as u8 & bits::MASK_DEV;
    let func = (addr >> 8) as u8 & bits::MASK_FUNC;
    // Register offset: bits 7:2 give the dword-aligned offset (bits 1:0 = 0)
    let reg = (addr & 0xFC) as u8;

    // dev and func are already masked to valid ranges
    let bdf = Bdf { bus, dev, func };
    Some((bdf, reg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_core::common::{ReadOp, WriteOp};

    #[test]
    fn bdf_valid() {
        let bdf = Bdf::new(0, 0, 0);
        assert!(bdf.is_some());
        let bdf = bdf.unwrap();
        assert_eq!(bdf.bus(), 0);
        assert_eq!(bdf.dev(), 0);
        assert_eq!(bdf.func(), 0);
    }

    #[test]
    fn bdf_max_valid() {
        let bdf = Bdf::new(255, 31, 7);
        assert!(bdf.is_some());
        let bdf = bdf.unwrap();
        assert_eq!(bdf.bus(), 255);
        assert_eq!(bdf.dev(), 31);
        assert_eq!(bdf.func(), 7);
    }

    #[test]
    fn bdf_dev_out_of_range() {
        assert!(Bdf::new(0, 32, 0).is_none());
    }

    #[test]
    fn bdf_func_out_of_range() {
        assert!(Bdf::new(0, 0, 8).is_none());
    }

    #[test]
    fn bdf_display() {
        let bdf = Bdf::new_unchecked(1, 2, 3);
        assert_eq!(bdf.to_string(), "1.2.3");
    }

    #[test]
    fn barn_try_from() {
        assert_eq!(BarN::try_from(0u8), Ok(BarN::BAR0));
        assert_eq!(BarN::try_from(5u8), Ok(BarN::BAR5));
        assert!(BarN::try_from(6u8).is_err());
    }

    #[test]
    fn cfg_addr_parse_enable_bit() {
        // Enable bit clear: should return None
        assert!(cfg_addr_parse(0x0000_0000).is_none());
        assert!(cfg_addr_parse(0x7FFF_FFFF).is_none());

        // Enable bit set
        let result = cfg_addr_parse(0x8000_0000);
        assert!(result.is_some());
    }

    #[test]
    fn cfg_addr_parse_fields() {
        // Bus=1, Dev=2, Func=3, Reg=0x10
        // 0x8000_0000 | (1 << 16) | (2 << 11) | (3 << 8) | 0x10
        let addr = 0x8001_1310;
        let (bdf, reg) = cfg_addr_parse(addr).unwrap();
        assert_eq!(bdf.bus(), 1);
        assert_eq!(bdf.dev(), 2);
        assert_eq!(bdf.func(), 3);
        assert_eq!(reg, 0x10);
    }

    #[test]
    fn cfg_addr_parse_reg_dword_aligned() {
        // Bits 1:0 should be masked off in the register offset
        let addr = 0x8000_0013; // reg bits = 0x13 => masked to 0x10
        let (_, reg) = cfg_addr_parse(addr).unwrap();
        assert_eq!(reg, 0x10);
    }

    #[test]
    fn pio_cfg_decoder_addr_readwrite() {
        let dec = PioCfgDecoder::new();
        assert_eq!(dec.addr(), 0);

        // Write an address
        let addr_val: u32 = 0x8001_1310;
        let wo = WriteOp::from_buf(&addr_val.to_le_bytes());
        dec.service_addr(0xCF8, RWOp::Write(&wo));
        assert_eq!(dec.addr(), addr_val);

        // Read it back
        let mut ro = ReadOp::new(4);
        dec.service_addr(0xCF8, RWOp::Read(&mut ro));
        let read_back = u32::from_le_bytes([
            ro.buf()[0],
            ro.buf()[1],
            ro.buf()[2],
            ro.buf()[3],
        ]);
        assert_eq!(read_back, addr_val);
    }

    #[test]
    fn service_addr_latches_dword() {
        let decoder = PioCfgDecoder::new();
        let addr = 0x8000_1000u32;
        let wo = WriteOp::from_buf(&addr.to_le_bytes());
        decoder.service_addr(0xCF8, RWOp::Write(&wo));

        let mut ro = ReadOp::new(4);
        decoder.service_addr(0xCF8, RWOp::Read(&mut ro));
        assert_eq!(u32::from_le_bytes(ro.buf().try_into().unwrap()), addr);

        let ignored = WriteOp::from_buf(&[0]);
        decoder.service_addr(0xCF9, RWOp::Write(&ignored));
        assert_eq!(decoder.addr(), addr);
    }

    #[test]
    fn pio_cfg_decoder_sub_dword_addr_ignored() {
        let dec = PioCfgDecoder::new();

        // Sub-dword writes to the address register should be ignored
        let wo = WriteOp::from_buf(&[0xFF, 0xFF]);
        dec.service_addr(0xCF8, RWOp::Write(&wo));
        assert_eq!(dec.addr(), 0);
    }

    #[test]
    fn pio_cfg_decoder_data_disabled() {
        let dec = PioCfgDecoder::new();
        // addr register = 0 (enable bit not set)

        let mut ro = ReadOp::new(4);
        dec.service_data(0xCFC, RWOp::Read(&mut ro), |_, _, _| {
            panic!("callback should not be called when disabled");
        });

        let val = u32::from_le_bytes([
            ro.buf()[0],
            ro.buf()[1],
            ro.buf()[2],
            ro.buf()[3],
        ]);
        assert_eq!(val, 0xFFFF_FFFF);
    }

    #[test]
    fn pio_cfg_decoder_data_dispatches() {
        let dec = PioCfgDecoder::new();

        // Set address: bus=0, dev=1, func=0, reg=0x04 (Command register)
        // 0x8000_0000 | (0 << 16) | (1 << 11) | (0 << 8) | 0x04
        let addr_val: u32 = 0x8000_0804;
        let wo = WriteOp::from_buf(&addr_val.to_le_bytes());
        dec.service_addr(0xCF8, RWOp::Write(&wo));

        let mut saw_bdf = None;
        let mut saw_offset = None;
        let mut ro = ReadOp::new(4);
        dec.service_data(0xCFC, RWOp::Read(&mut ro), |bdf, off, rwo| {
            saw_bdf = Some(*bdf);
            saw_offset = Some(off);
            if let RWOp::Read(ro) = rwo {
                ro.write_u32(0x1234_5678);
            }
        });

        let bdf = saw_bdf.unwrap();
        assert_eq!(bdf.bus(), 0);
        assert_eq!(bdf.dev(), 1);
        assert_eq!(bdf.func(), 0);
        assert_eq!(saw_offset.unwrap(), 0x04);
    }

    #[test]
    fn pio_cfg_decoder_byte_offset() {
        let dec = PioCfgDecoder::new();

        // Set address: bus=0, dev=0, func=0, reg=0x04
        let addr_val: u32 = 0x8000_0004;
        let wo = WriteOp::from_buf(&addr_val.to_le_bytes());
        dec.service_addr(0xCF8, RWOp::Write(&wo));

        // Access port 0xCFE = 0xCFC + 2, so byte offset = 2
        // Total config offset = 0x04 + 2 = 0x06
        let mut saw_offset = 0u8;
        let mut ro = ReadOp::new(1);
        dec.service_data(0xCFE, RWOp::Read(&mut ro), |_, off, _| {
            saw_offset = off;
        });
        assert_eq!(saw_offset, 0x06);
    }
}
