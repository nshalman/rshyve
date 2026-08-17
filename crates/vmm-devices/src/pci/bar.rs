// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Portions derived from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream: lib/propolis/src/hw/pci/bar.rs
// https://github.com/oxidecomputer/propolis

//! PCI Base Address Register (BAR) management.
//!
//! Handles the BAR sizing protocol: the guest writes all-ones to a BAR,
//! reads back the size mask, then writes the assigned base address.
//! Type bits in the low bits of each BAR register are always read-only.

use super::bits;
use super::BarN;

/// Number of BAR slots in a Type 0 PCI device header.
pub const BAR_COUNT: usize = 6;

/// Defines the type and size of a BAR.
///
/// The size must be a power of two and represents the minimum alignment
/// requirement for the BAR's address.
#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum BarDefine {
    /// I/O port BAR with the given size in bytes.
    Pio(u16),
    /// 32-bit memory BAR with the given size in bytes.
    Mmio(u32),
    /// 64-bit memory BAR with the given size in bytes.
    /// Consumes two consecutive BAR slots.
    Mmio64(u64),
}

impl BarDefine {
    /// Returns `true` if this is a PIO BAR.
    pub fn is_pio(&self) -> bool {
        matches!(self, BarDefine::Pio(_))
    }

    /// Returns `true` if this is an MMIO BAR (32-bit or 64-bit).
    pub fn is_mmio(&self) -> bool {
        matches!(self, BarDefine::Mmio(_) | BarDefine::Mmio64(_))
    }

    /// Returns the size of this BAR in bytes.
    pub fn size(&self) -> u64 {
        match self {
            BarDefine::Pio(sz) => u64::from(*sz),
            BarDefine::Mmio(sz) => u64::from(*sz),
            BarDefine::Mmio64(sz) => *sz,
        }
    }
}

/// Internal entry kind tracking.
#[derive(Copy, Clone)]
enum EntryKind {
    /// Slot is unused.
    Empty,
    /// I/O port BAR with given size.
    Pio(u16),
    /// 32-bit memory BAR with given size.
    Mmio(u32),
    /// 64-bit memory BAR with given size (stored in the low slot).
    Mmio64(u64),
    /// Upper 32 bits of a 64-bit memory BAR (always follows an Mmio64 entry).
    Mmio64High,
}

/// Internal BAR entry: kind + current address value.
#[derive(Copy, Clone)]
struct Entry {
    kind: EntryKind,
    value: u64,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            kind: EntryKind::Empty,
            value: 0,
        }
    }
}

/// Result from a write to a BAR register.
#[derive(Debug)]
pub struct BarWriteResult {
    /// Identifier of the actual impacted BAR.
    ///
    /// If the write was to the high word of a 64-bit BAR, this holds the
    /// `BarN` for the lower (defining) slot.
    pub bar: BarN,
    /// The BAR definition (type + size).
    pub def: BarDefine,
    /// Previous base address.
    pub old_addr: u64,
    /// New base address.
    pub new_addr: u64,
}

/// Manages the six BAR registers of a PCI Type 0 device.
pub struct Bars {
    entries: [Entry; BAR_COUNT],
}

impl Bars {
    /// Create a new BAR set with no BARs defined.
    pub fn new() -> Self {
        Self {
            entries: Default::default(),
        }
    }

    /// Define a BAR.
    ///
    /// # Panics
    ///
    /// - If the BAR slot is already in use.
    /// - If a 64-bit BAR would not fit (BAR5 cannot be the low half of a
    ///   64-bit BAR because there is no BAR6).
    /// - If the size is not a power of two.
    pub fn define(&mut self, bar: BarN, def: BarDefine) {
        let idx = bar as usize;
        assert!(
            matches!(self.entries[idx].kind, EntryKind::Empty),
            "BAR{} is already defined",
            idx
        );

        match def {
            BarDefine::Pio(sz) => {
                assert!(
                    sz.is_power_of_two(),
                    "PIO BAR size must be power of 2"
                );
                self.entries[idx].kind = EntryKind::Pio(sz);
            }
            BarDefine::Mmio(sz) => {
                assert!(
                    sz.is_power_of_two(),
                    "MMIO BAR size must be power of 2"
                );
                self.entries[idx].kind = EntryKind::Mmio(sz);
            }
            BarDefine::Mmio64(sz) => {
                assert!(
                    sz.is_power_of_two(),
                    "MMIO64 BAR size must be power of 2"
                );
                assert!(
                    idx < (BarN::BAR5 as usize),
                    "64-bit BAR cannot start at BAR5"
                );
                assert!(
                    matches!(self.entries[idx + 1].kind, EntryKind::Empty),
                    "BAR{} (high half for 64-bit) is already defined",
                    idx + 1
                );
                self.entries[idx].kind = EntryKind::Mmio64(sz);
                self.entries[idx + 1].kind = EntryKind::Mmio64High;
            }
        }
    }

    /// Read the BAR register value for the given slot.
    ///
    /// Returns the current address OR'd with the type bits.
    /// Empty BARs read as zero.
    pub fn reg_read(&self, bar: BarN) -> u32 {
        let idx = bar as usize;
        let ent = self.entries[idx];
        match ent.kind {
            EntryKind::Empty => 0,
            EntryKind::Pio(_) => {
                u32::from(ent.value as u16) | bits::BAR_TYPE_IO
            }
            EntryKind::Mmio(_) => ent.value as u32 | bits::BAR_TYPE_MEM,
            EntryKind::Mmio64(_) => ent.value as u32 | bits::BAR_TYPE_MEM64,
            EntryKind::Mmio64High => {
                assert_ne!(idx, 0);
                let ent = self.entries[idx - 1];
                assert!(matches!(ent.kind, EntryKind::Mmio64(_)));
                (ent.value >> 32) as u32
            }
        }
    }

    /// Write a value to a BAR register.
    ///
    /// The written value is masked by the BAR's size alignment. Type bits
    /// in the low bits are preserved (read-only).
    ///
    /// Returns `Some(BarWriteResult)` if the address actually changed,
    /// or `None` if the BAR is empty or the address did not change.
    pub fn reg_write(&mut self, bar: BarN, val: u32) -> Option<BarWriteResult> {
        let idx = bar as usize;
        let ent = &mut self.entries[idx];
        let (id, def, val_old, val_new) = match ent.kind {
            EntryKind::Empty => return None,
            EntryKind::Pio(size) => {
                let mask = u32::from(!(size - 1));
                let old = ent.value;
                ent.value = u64::from(val & mask);
                (bar, BarDefine::Pio(size), old, ent.value)
            }
            EntryKind::Mmio(size) => {
                let mask = !(size - 1);
                let old = ent.value;
                ent.value = u64::from(val & mask);
                (bar, BarDefine::Mmio(size), old, ent.value)
            }
            EntryKind::Mmio64(size) => {
                let old = ent.value;
                let mask = !(size - 1) as u32;
                let low = val & mask;
                ent.value = (old & (0xFFFF_FFFF << 32)) | u64::from(low);
                (bar, BarDefine::Mmio64(size), old, ent.value)
            }
            EntryKind::Mmio64High => {
                assert!(idx > 0);
                let real_idx = idx - 1;
                let id =
                    BarN::try_from(real_idx as u8).expect("valid BAR index");
                let ent = &mut self.entries[real_idx];
                let size = match ent.kind {
                    EntryKind::Mmio64(sz) => sz,
                    _ => panic!("Mmio64High without preceding Mmio64"),
                };
                let mask = !(size - 1);
                let old = ent.value;
                let high =
                    ((u64::from(val) << 32) & mask) & 0xFFFF_FFFF_0000_0000;
                ent.value = high | (old & 0xFFFF_FFFF);
                (id, BarDefine::Mmio64(size), old, ent.value)
            }
        };

        if val_old != val_new {
            Some(BarWriteResult {
                bar: id,
                def,
                old_addr: val_old,
                new_addr: val_new,
            })
        } else {
            None
        }
    }

    /// Get the BAR definition and current address for a given slot.
    ///
    /// Returns `None` for empty or Mmio64High slots.
    pub fn get(&self, bar: BarN) -> Option<(BarDefine, u64)> {
        let ent = &self.entries[bar as usize];
        let def = match ent.kind {
            EntryKind::Empty | EntryKind::Mmio64High => return None,
            EntryKind::Pio(sz) => BarDefine::Pio(sz),
            EntryKind::Mmio(sz) => BarDefine::Mmio(sz),
            EntryKind::Mmio64(sz) => BarDefine::Mmio64(sz),
        };
        Some((def, ent.value))
    }

    /// Directly set the address value for a defined BAR.
    ///
    /// # Panics
    ///
    /// - If the BAR is not defined (empty or Mmio64High).
    /// - If the value exceeds the BAR type's address range.
    pub fn set(&mut self, bar: BarN, value: u64) {
        let ent = &mut self.entries[bar as usize];
        match ent.kind {
            EntryKind::Empty => {
                panic!("{:?} not defined", bar);
            }
            EntryKind::Mmio64High => {
                panic!("high BAR bits must not be set directly");
            }
            EntryKind::Pio(_) => {
                assert!(value <= u64::from(u16::MAX));
            }
            EntryKind::Mmio(_) => {
                assert!(value <= u64::from(u32::MAX));
            }
            EntryKind::Mmio64(_) => {}
        }
        ent.value = value;
    }
}

impl Default for Bars {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Bars {
        let mut bars = Bars::new();
        bars.define(BarN::BAR0, BarDefine::Pio(0x100));
        bars.define(BarN::BAR1, BarDefine::Mmio(0x20000));
        bars.define(BarN::BAR2, BarDefine::Mmio64(0x40000));
        // BAR3 consumed by Mmio64High
        bars.define(BarN::BAR4, BarDefine::Mmio64(0x2_0000_0000));
        // BAR5 consumed by Mmio64High
        bars
    }

    #[test]
    fn init() {
        let _ = setup();
    }

    #[test]
    fn empty_bar_reads_zero() {
        let bars = Bars::new();
        assert_eq!(bars.reg_read(BarN::BAR0), 0);
        assert_eq!(bars.reg_read(BarN::BAR3), 0);
    }

    #[test]
    fn read_type_bits() {
        let mut bars = setup();
        bars.set(BarN::BAR0, 0x1000);
        bars.set(BarN::BAR1, 0xC00_0000);
        bars.set(BarN::BAR2, 0xD00_0000);
        bars.set(BarN::BAR4, 0x8_0000_0000);

        // PIO: low bit is 1
        assert_eq!(bars.reg_read(BarN::BAR0), 0x1001);
        // MMIO 32-bit: low bits are 0b000
        assert_eq!(bars.reg_read(BarN::BAR1), 0x0C00_0000);
        // MMIO 64-bit: low bits are 0b100
        assert_eq!(bars.reg_read(BarN::BAR2), 0x0D00_0004);
        // MMIO64 high half
        assert_eq!(bars.reg_read(BarN::BAR3), 0);
        // Large 64-bit BAR
        assert_eq!(bars.reg_read(BarN::BAR4), 0x0000_0004);
        assert_eq!(bars.reg_read(BarN::BAR5), 0x0000_0008);
    }

    #[test]
    fn write_and_read_back() {
        let mut bars = setup();
        bars.reg_write(BarN::BAR0, 0x1000);
        bars.reg_write(BarN::BAR1, 0xC00_0000);
        bars.reg_write(BarN::BAR2, 0xD00_0000);
        bars.reg_write(BarN::BAR5, 0x8);
        bars.reg_write(BarN::BAR4, 0x0);

        assert_eq!(bars.reg_read(BarN::BAR0), 0x1001);
        assert_eq!(bars.reg_read(BarN::BAR1), 0x0C00_0000);
        assert_eq!(bars.reg_read(BarN::BAR2), 0x0D00_0004);
        assert_eq!(bars.reg_read(BarN::BAR3), 0);
        assert_eq!(bars.reg_read(BarN::BAR4), 0x0000_0004);
        assert_eq!(bars.reg_read(BarN::BAR5), 0x0000_0008);
    }

    #[test]
    fn size_mask_protocol() {
        let mut bars = setup();

        // Write all-ones to probe size
        for i in 0..=5u8 {
            bars.reg_write(BarN::try_from(i).unwrap(), 0xFFFF_FFFF);
        }

        // PIO size 0x100: mask = ~(0x100 - 1) = 0xFFFF_FF00, | IO bit
        assert_eq!(bars.reg_read(BarN::BAR0), 0x0000_FF01);
        // MMIO size 0x20000: mask = ~(0x20000 - 1) = 0xFFFE_0000
        assert_eq!(bars.reg_read(BarN::BAR1), 0xFFFE_0000);
        // MMIO64 size 0x40000: low mask = ~(0x40000 - 1) = 0xFFFC_0000, | 0x4
        assert_eq!(bars.reg_read(BarN::BAR2), 0xFFFC_0004);
        // High half: 0xFFFF_FFFF & high mask
        assert_eq!(bars.reg_read(BarN::BAR3), 0xFFFF_FFFF);
        // MMIO64 size 0x2_0000_0000: low mask = 0, | 0x4
        assert_eq!(bars.reg_read(BarN::BAR4), 0x0000_0004);
        // High half
        assert_eq!(bars.reg_read(BarN::BAR5), 0xFFFF_FFFE);
    }

    #[test]
    fn write_empty_bar_returns_none() {
        let mut bars = Bars::new();
        assert!(bars.reg_write(BarN::BAR0, 0x1000).is_none());
    }

    #[test]
    fn write_result_reports_change() {
        let mut bars = Bars::new();
        bars.define(BarN::BAR0, BarDefine::Pio(0x100));

        let result = bars.reg_write(BarN::BAR0, 0x1000);
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.bar, BarN::BAR0);
        assert_eq!(r.old_addr, 0);
        assert_eq!(r.new_addr, 0x1000);

        // Same write again: no change
        assert!(bars.reg_write(BarN::BAR0, 0x1000).is_none());
    }

    #[test]
    fn get_returns_definition_and_value() {
        let mut bars = Bars::new();
        bars.define(BarN::BAR1, BarDefine::Mmio(0x1000));
        bars.set(BarN::BAR1, 0xFEE0_0000);

        let (def, val) = bars.get(BarN::BAR1).unwrap();
        assert_eq!(def, BarDefine::Mmio(0x1000));
        assert_eq!(val, 0xFEE0_0000);
    }

    #[test]
    fn get_empty_returns_none() {
        let bars = Bars::new();
        assert!(bars.get(BarN::BAR0).is_none());
    }

    #[test]
    #[should_panic(expected = "already defined")]
    fn define_duplicate_panics() {
        let mut bars = Bars::new();
        bars.define(BarN::BAR0, BarDefine::Pio(0x100));
        bars.define(BarN::BAR0, BarDefine::Pio(0x200));
    }

    #[test]
    #[should_panic(expected = "cannot start at BAR5")]
    fn mmio64_at_bar5_panics() {
        let mut bars = Bars::new();
        bars.define(BarN::BAR5, BarDefine::Mmio64(0x1000));
    }

    #[test]
    #[should_panic(expected = "power of 2")]
    fn non_power_of_two_panics() {
        let mut bars = Bars::new();
        bars.define(BarN::BAR0, BarDefine::Pio(0x300));
    }
}
