// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI bus with 32 device slots and 8 functions per slot.
//!
//! The bus dispatches config space reads/writes to attached devices.
//! Empty slots return all-ones (0xFFFF_FFFF), matching the x86 bus
//! float behavior that UEFI relies on when scanning for devices.

use std::sync::{Arc, Mutex};

use thiserror::Error;

use super::bits;
use super::device::PciDevice;
use super::Bdf;

const SLOTS_PER_BUS: usize = 32;
const FUNCS_PER_SLOT: usize = 8;

type PciSlot = [Option<Arc<dyn PciDevice>>; FUNCS_PER_SLOT];
type PciSlots = [PciSlot; SLOTS_PER_BUS];

/// Reason an attach request was refused.
#[derive(Debug, Error)]
pub enum AttachError {
    /// Another device holds this BDF.
    #[error("device already attached at BDF {0}")]
    SlotOccupied(Bdf),
}

/// PCI bus holding up to 32 device slots, each with up to 8 functions.
pub struct PciBus {
    inner: Mutex<PciBusInner>,
}

struct PciBusInner {
    /// 32 slots x 8 functions.
    slots: Box<PciSlots>,
}

/// Set or clear the multi-function bit in function 0's header type.
///
/// The caller must not hold the bus lock: a device config access can
/// re-enter the PIO/MMIO buses.
fn set_multifunc(func0: &Arc<dyn PciDevice>, multifunc: bool) {
    let cur = func0.cfg_read(bits::REG_HEADER_TYPE, 1) as u8;
    let new = if multifunc {
        cur | bits::HEADER_TYPE_MULTIFUNC
    } else {
        cur & !bits::HEADER_TYPE_MULTIFUNC
    };
    if new != cur {
        func0.cfg_write(bits::REG_HEADER_TYPE, 1, u32::from(new));
    }
}

impl PciBus {
    /// Create a new, empty PCI bus.
    pub fn new() -> Arc<Self> {
        // Use Box to avoid a large stack allocation (32 * 8 * sizeof(Option<Arc>)).
        let slots =
            Box::new(std::array::from_fn(|_| std::array::from_fn(|_| None)));
        Arc::new(Self {
            inner: Mutex::new(PciBusInner { slots }),
        })
    }

    /// Attach a PCI device at the given bus/device/function address.
    ///
    /// # Panics
    ///
    /// Panics if a device is already attached at this BDF. Use
    /// [`try_attach`](Self::try_attach) when the BDF comes from a
    /// request at run time.
    pub fn attach(&self, bdf: Bdf, dev: Arc<dyn PciDevice>) {
        if let Err(e) = self.try_attach(bdf, dev) {
            panic!("{e}");
        }
    }

    /// Attach a PCI device, returning an error if the BDF is occupied.
    pub fn try_attach(
        &self,
        bdf: Bdf,
        dev: Arc<dyn PciDevice>,
    ) -> Result<(), AttachError> {
        let func0 = {
            let mut inner = self.inner.lock().expect("PciBus lock poisoned");
            let slot =
                &mut inner.slots[bdf.dev() as usize][bdf.func() as usize];
            if slot.is_some() {
                return Err(AttachError::SlotOccupied(bdf));
            }
            *slot = Some(dev);

            // A device at function > 0 is only scanned by the guest when
            // function 0 reports multi-function.
            if bdf.func() > 0 {
                inner.slots[bdf.dev() as usize][0].clone()
            } else {
                None
            }
        };

        if let Some(func0) = func0 {
            set_multifunc(&func0, true);
        }
        Ok(())
    }

    /// Remove the device at the given BDF and return it.
    ///
    /// Returns `None` if the slot is empty.
    pub fn detach(&self, bdf: &Bdf) -> Option<Arc<dyn PciDevice>> {
        let mut inner = self.inner.lock().expect("PciBus lock poisoned");
        let dev =
            inner.slots[bdf.dev() as usize][bdf.func() as usize].take()?;

        // The slot stops being multi-function when the last function > 0
        // leaves.
        let func0 = if bdf.func() > 0
            && inner.slots[bdf.dev() as usize][1..]
                .iter()
                .all(Option::is_none)
        {
            inner.slots[bdf.dev() as usize][0].clone()
        } else {
            None
        };
        drop(inner);

        if let Some(func0) = func0 {
            set_multifunc(&func0, false);
        }
        Some(dev)
    }

    /// Read from PCI configuration space.
    ///
    /// Returns all-ones (0xFFFF_FFFF) for empty slots, which is the
    /// correct bus-float response that UEFI expects when scanning.
    pub fn config_read(&self, bdf: &Bdf, offset: u8, len: u8) -> u32 {
        let inner = self.inner.lock().expect("PciBus lock poisoned");
        match &inner.slots[bdf.dev() as usize][bdf.func() as usize] {
            Some(dev) => {
                let dev = Arc::clone(dev);
                drop(inner);
                dev.cfg_read(offset, len)
            }
            None => {
                // Bus float: all ones
                match len {
                    1 => 0xFF,
                    2 => 0xFFFF,
                    _ => 0xFFFF_FFFF,
                }
            }
        }
    }

    /// Write to PCI configuration space.
    ///
    /// Writes to empty slots are silently dropped.
    pub fn config_write(&self, bdf: &Bdf, offset: u8, len: u8, val: u32) {
        let inner = self.inner.lock().expect("PciBus lock poisoned");
        if let Some(dev) = &inner.slots[bdf.dev() as usize][bdf.func() as usize]
        {
            let dev = Arc::clone(dev);
            drop(inner);
            dev.cfg_write(offset, len, val);
        }
        // Writes to empty slots are silently ignored.
    }

    /// Look up the device at the given BDF, if any.
    pub fn device_at(&self, bdf: &Bdf) -> Option<Arc<dyn PciDevice>> {
        let inner = self.inner.lock().expect("PciBus lock poisoned");
        inner.slots[bdf.dev() as usize][bdf.func() as usize].clone()
    }
}

impl Default for PciBus {
    fn default() -> Self {
        // Return the inner PciBus, not an Arc<PciBus>
        let slots =
            Box::new(std::array::from_fn(|_| std::array::from_fn(|_| None)));
        Self {
            inner: Mutex::new(PciBusInner { slots }),
        }
    }
}

impl std::fmt::Debug for PciBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PciBus").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_core::common::RWOp;

    /// Minimal test device that responds with its vendor/device ID.
    struct TestDevice {
        vendor_id: u16,
        device_id: u16,
    }

    impl PciDevice for TestDevice {
        fn cfg_read(&self, offset: u8, len: u8) -> u32 {
            match (offset, len) {
                (0x00, 4) => {
                    u32::from(self.vendor_id)
                        | (u32::from(self.device_id) << 16)
                }
                (0x00, 2) => u32::from(self.vendor_id),
                (0x02, 2) => u32::from(self.device_id),
                _ => 0,
            }
        }

        fn cfg_write(&self, _offset: u8, _len: u8, _val: u32) {}

        fn bar_rw(&self, _bar: BarN, _offset: usize, _rwo: RWOp<'_>) {}
    }

    use super::super::BarN;

    /// Test device that stores the header type byte written to it.
    struct HeaderDevice {
        header_type: Mutex<u8>,
    }

    impl HeaderDevice {
        fn new() -> Self {
            Self {
                header_type: Mutex::new(bits::HEADER_TYPE_DEVICE),
            }
        }

        fn header_type(&self) -> u8 {
            *self.header_type.lock().expect("header lock poisoned")
        }
    }

    impl PciDevice for HeaderDevice {
        fn cfg_read(&self, offset: u8, _len: u8) -> u32 {
            match offset {
                bits::REG_HEADER_TYPE => u32::from(self.header_type()),
                _ => 0,
            }
        }

        fn cfg_write(&self, offset: u8, _len: u8, val: u32) {
            if offset == bits::REG_HEADER_TYPE {
                *self.header_type.lock().expect("header lock poisoned") =
                    val as u8;
            }
        }

        fn bar_rw(&self, _bar: BarN, _offset: usize, _rwo: RWOp<'_>) {}
    }

    #[test]
    fn empty_slot_returns_all_ones() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 5, 0);
        assert_eq!(bus.config_read(&bdf, 0x00, 4), 0xFFFF_FFFF);
        assert_eq!(bus.config_read(&bdf, 0x00, 2), 0xFFFF);
        assert_eq!(bus.config_read(&bdf, 0x00, 1), 0xFF);
    }

    #[test]
    fn all_slots_empty_on_creation() {
        let bus = PciBus::new();
        for dev in 0..32u8 {
            for func in 0..8u8 {
                let bdf = Bdf::new_unchecked(0, dev, func);
                assert_eq!(
                    bus.config_read(&bdf, 0x00, 4),
                    0xFFFF_FFFF,
                    "expected bus float at {}.{}.{}",
                    0,
                    dev,
                    func,
                );
            }
        }
    }

    #[test]
    fn attach_and_read() {
        let bus = PciBus::new();
        let dev: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x8086,
            device_id: 0x1237,
        });
        let bdf = Bdf::new_unchecked(0, 0, 0);
        bus.attach(bdf, dev);

        let val = bus.config_read(&bdf, 0x00, 4);
        assert_eq!(val & 0xFFFF, 0x8086);
        assert_eq!((val >> 16) & 0xFFFF, 0x1237);
    }

    #[test]
    fn write_to_empty_slot_is_silent() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 10, 0);
        // Should not panic
        bus.config_write(&bdf, 0x04, 2, 0x0003);
    }

    #[test]
    fn device_at_returns_some_for_attached() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 2, 0);
        let dev: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x1234,
            device_id: 0x5678,
        });
        bus.attach(bdf, dev);
        assert!(bus.device_at(&bdf).is_some());
    }

    #[test]
    fn device_at_returns_none_for_empty() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 3, 0);
        assert!(bus.device_at(&bdf).is_none());
    }

    #[test]
    #[should_panic(expected = "already attached")]
    fn double_attach_panics() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 0, 0);
        let dev1: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x1111,
            device_id: 0x2222,
        });
        let dev2: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x3333,
            device_id: 0x4444,
        });
        bus.attach(bdf, dev1);
        bus.attach(bdf, dev2);
    }

    #[test]
    fn try_attach_occupied_keeps_first_device() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 0, 0);
        let dev1: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x1111,
            device_id: 0x2222,
        });
        let dev2: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x3333,
            device_id: 0x4444,
        });
        bus.try_attach(bdf, dev1).expect("first attach");

        let err = bus.try_attach(bdf, dev2).expect_err("slot is occupied");
        assert!(matches!(err, AttachError::SlotOccupied(b) if b == bdf));
        assert_eq!(bus.config_read(&bdf, 0x00, 2), 0x1111);
    }

    #[test]
    fn detach_returns_the_device() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 6, 0);
        let dev: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x1AF4,
            device_id: 0x1000,
        });
        bus.attach(bdf, Arc::clone(&dev));

        let removed = bus.detach(&bdf).expect("device was attached");
        assert!(Arc::ptr_eq(&removed, &dev));
        assert!(bus.device_at(&bdf).is_none());
        assert_eq!(bus.config_read(&bdf, 0x00, 4), 0xFFFF_FFFF);
    }

    #[test]
    fn detach_empty_slot_returns_none() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 7, 0);
        assert!(bus.detach(&bdf).is_none());
    }

    #[test]
    fn attach_at_func1_sets_multifunc() {
        let bus = PciBus::new();
        let func0 = Arc::new(HeaderDevice::new());
        bus.attach(Bdf::new_unchecked(0, 8, 0), func0.clone());
        assert_eq!(func0.header_type(), bits::HEADER_TYPE_DEVICE);

        bus.attach(Bdf::new_unchecked(0, 8, 1), Arc::new(HeaderDevice::new()));
        assert_eq!(
            func0.header_type() & bits::HEADER_TYPE_MULTIFUNC,
            bits::HEADER_TYPE_MULTIFUNC
        );
    }

    #[test]
    fn detach_of_last_upper_func_clears_multifunc() {
        let bus = PciBus::new();
        let func0 = Arc::new(HeaderDevice::new());
        let bdf1 = Bdf::new_unchecked(0, 9, 1);
        let bdf2 = Bdf::new_unchecked(0, 9, 2);
        bus.attach(Bdf::new_unchecked(0, 9, 0), func0.clone());
        bus.attach(bdf1, Arc::new(HeaderDevice::new()));
        bus.attach(bdf2, Arc::new(HeaderDevice::new()));

        // One function > 0 remains, so the slot is still multi-function.
        bus.detach(&bdf2).expect("func 2 was attached");
        assert_eq!(
            func0.header_type() & bits::HEADER_TYPE_MULTIFUNC,
            bits::HEADER_TYPE_MULTIFUNC
        );

        bus.detach(&bdf1).expect("func 1 was attached");
        assert_eq!(func0.header_type(), bits::HEADER_TYPE_DEVICE);
    }

    #[test]
    fn multiple_devices_independent() {
        let bus = PciBus::new();
        let bdf0 = Bdf::new_unchecked(0, 0, 0);
        let bdf1 = Bdf::new_unchecked(0, 1, 0);

        let dev0: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0xAAAA,
            device_id: 0xBBBB,
        });
        let dev1: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0xCCCC,
            device_id: 0xDDDD,
        });
        bus.attach(bdf0, dev0);
        bus.attach(bdf1, dev1);

        assert_eq!(bus.config_read(&bdf0, 0x00, 2), 0xAAAA);
        assert_eq!(bus.config_read(&bdf1, 0x00, 2), 0xCCCC);

        // Unoccupied slot still returns all-ones
        let bdf_empty = Bdf::new_unchecked(0, 2, 0);
        assert_eq!(bus.config_read(&bdf_empty, 0x00, 4), 0xFFFF_FFFF);
    }
}
