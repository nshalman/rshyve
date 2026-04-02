// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO PCI transport (legacy).
//!
//! Wraps a [`VirtioDevice`] and exposes it as a PCI device using the
//! legacy (pre-1.0) PCI transport mechanism. The device registers a
//! PIO BAR (BAR0) containing the common virtio configuration registers
//! followed by device-specific configuration.
//!
//! Legacy register layout in BAR0:
//! ```text
//! 0x00  device_features   (4B, read)
//! 0x04  guest_features    (4B, write)
//! 0x08  queue_pfn         (4B, read/write)
//! 0x0C  queue_size        (2B, read)
//! 0x0E  queue_select      (2B, write)
//! 0x10  queue_notify      (2B, write)
//! 0x12  device_status     (1B, read/write)
//! 0x13  isr_status        (1B, read)
//! 0x14+ device-specific config
//! ```

use std::sync::{Arc, Mutex};

use vmm_core::common::RWOp;
use vmm_core::mem::PhysMap;
use vmm_core::pio::{PioBus, PioFn};

use crate::pci::bar::BarDefine;
use crate::pci::device::{DeviceIdent, DeviceState, PciDevice};
use crate::pci::{BarN, IntrPin, LintrCfg};

use super::bits;
use super::queue::VirtQueue;
use super::VirtioDevice;

/// Maximum number of virtqueues per device.
const MAX_QUEUES: usize = 4;

/// State shared between the PCI device and the VirtIO transport.
struct VirtioPciState {
    /// Selected queue index for configuration.
    queue_select: u16,
    /// Device status register.
    status: u8,
    /// ISR status (read clears).
    isr_status: u8,
    /// Features negotiated by the guest.
    guest_features: u32,
    /// Device-owned virtqueues.
    queues: Vec<VirtQueue>,
}

impl VirtioPciState {
    fn new(num_queues: usize, queue_size: u16) -> Self {
        let queues = (0..num_queues)
            .map(|_| VirtQueue::new(queue_size))
            .collect();
        Self {
            queue_select: 0,
            status: 0,
            isr_status: 0,
            guest_features: 0,
            queues,
        }
    }

    fn selected_queue(&self) -> Option<&VirtQueue> {
        self.queues.get(self.queue_select as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut VirtQueue> {
        self.queues.get_mut(self.queue_select as usize)
    }

    fn reset(&mut self) {
        self.queue_select = 0;
        self.status = 0;
        self.isr_status = 0;
        self.guest_features = 0;
        for q in &mut self.queues {
            q.reset();
        }
    }
}

/// PCI device wrapping a VirtIO backend.
///
/// Implements the [`PciDevice`] trait so it can be attached to the PCI
/// bus. BAR0 is a PIO region containing the legacy virtio config space.
pub struct VirtioPciDevice<D: VirtioDevice> {
    /// Standard PCI config header state.
    pci_state: Mutex<DeviceState>,
    /// VirtIO transport state.
    virtio_state: Mutex<VirtioPciState>,
    /// The backend device (e.g., block, net).
    device: D,
    /// Interrupt pin for legacy INTx delivery.
    intr_pin: Option<Arc<dyn IntrPin>>,
    /// Reference to guest physical memory for queue processing.
    physmap: Arc<PhysMap>,
    /// PIO bus for BAR registration/unregistration.
    bus_pio: Arc<PioBus>,
    /// Currently registered BAR base port (if any).
    registered_bar: Mutex<Option<u16>>,
    /// Weak self-reference for BAR registration closures.
    self_ref: Mutex<Option<std::sync::Weak<Self>>>,
}

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// Create a new VirtIO PCI device.
    ///
    /// # Arguments
    ///
    /// - `device` - The VirtIO backend implementation
    /// - `dev_type` - VirtIO device type (e.g., VIRTIO_DEV_TYPE_BLOCK)
    /// - `num_queues` - Number of virtqueues
    /// - `queue_size` - Size of each virtqueue (must be power of 2)
    /// - `dev_config_size` - Size of device-specific config region
    /// - `lintr_cfg` - Legacy interrupt pin configuration
    /// - `physmap` - Guest physical memory map
    pub fn new(
        device: D,
        dev_type: u16,
        num_queues: usize,
        queue_size: u16,
        dev_config_size: u16,
        lintr_cfg: Option<LintrCfg>,
        physmap: Arc<PhysMap>,
        bus_pio: Arc<PioBus>,
    ) -> Arc<Self> {
        assert!(
            num_queues <= MAX_QUEUES,
            "too many queues (max {})",
            MAX_QUEUES
        );

        // PCI device ID for legacy virtio: base + device type
        // Network=0x1000, Block=0x1001, etc.
        let pci_device_id = 0x1000 + dev_type - 1;

        let pci_class = match dev_type {
            bits::VIRTIO_DEV_TYPE_NET => crate::pci::bits::CLASS_NETWORK,
            bits::VIRTIO_DEV_TYPE_BLOCK => crate::pci::bits::CLASS_STORAGE,
            _ => crate::pci::bits::CLASS_UNCLASSIFIED,
        };

        let ident = DeviceIdent {
            vendor_id: bits::VIRTIO_PCI_VENDOR_ID,
            device_id: pci_device_id,
            class: pci_class,
            subclass: 0,
            prog_if: 0,
            revision: 0,
            sub_vendor_id: bits::VIRTIO_PCI_VENDOR_ID,
            sub_device_id: dev_type,
        };

        let mut pci_state = DeviceState::new(ident);

        // BAR0: PIO for virtio config space
        // Size = common config (0x14) + device-specific config
        let bar_size = bits::LEGACY_COMMON_SIZE + dev_config_size;
        // Round up to power of 2 for PCI BAR alignment
        let bar_size_aligned = bar_size.next_power_of_two().max(32);
        pci_state.define_bar(BarN::BAR0, BarDefine::Pio(bar_size_aligned));

        // Set interrupt pin and line register
        let intr_pin = if let Some((pin_id, pin)) = lintr_cfg {
            pci_state.set_intr_pin(pin_id as u8);
            // Pre-set the interrupt line register. The kernel reads
            // this to determine the IRQ when ACPI _PRT is unavailable.
            pci_state.set_intr_line(10); // Default IRQ 10 for PCI devices
            Some(pin)
        } else {
            None
        };

        let dev = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            virtio_state: Mutex::new(VirtioPciState::new(
                num_queues,
                queue_size,
            )),
            device,
            intr_pin,
            physmap,
            bus_pio,
            registered_bar: Mutex::new(None),
            self_ref: Mutex::new(None),
        });
        // Store weak self-reference for BAR registration
        *dev.self_ref.lock().expect("self_ref lock") =
            Some(Arc::downgrade(&dev));
        dev
    }

    /// Update PIO bus registration for BAR0 based on current state.
    ///
    /// Must be called with an Arc<Self> available. Registers or
    /// unregisters the PIO handler as needed.
    fn update_bar_registration(
        self: &Arc<Self>,
        pci: &DeviceState,
    ) {
        let io_enabled =
            pci.command().contains(crate::pci::bits::RegCmd::IO_EN);
        let bar_info = pci.bars().get(BarN::BAR0);

        let mut reg = self.registered_bar.lock()
            .expect("bar lock poisoned");

        // Unregister old handler if present
        if let Some(old_port) = reg.take() {
            let _ = self.bus_pio.unregister(old_port);
        }

        // Register new handler if IO is enabled and BAR has an address
        if io_enabled {
            if let Some((BarDefine::Pio(size), addr)) = bar_info {
                let port = addr as u16;
                if port != 0 {
                    let dev = Arc::clone(self);
                    let handler: Arc<PioFn> =
                        Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                            dev.bar_rw(
                                BarN::BAR0,
                                offset as usize,
                                rwo,
                            );
                        });
                    match self.bus_pio.register(port, size, handler) {
                        Ok(()) => {
                            *reg = Some(port);
                        }
                        Err(_) => {}
                    }
                }
            }
        }
    }

    /// Assert the legacy interrupt to notify the guest.
    ///
    /// Uses edge-triggered delivery: deassert then reassert to generate
    /// a new edge. The PciIntrPin calls `vm_isa_assert_irq(atpic, ioapic)`
    /// which delivers through both the legacy PIC and IOAPIC paths.
    fn raise_interrupt(&self) {
        if let Some(pin) = &self.intr_pin {
            pin.deassert();
            pin.assert();
        }
    }

    /// Deassert the interrupt after ISR is read or on device reset.
    fn lower_interrupt(&self) {
        if let Some(pin) = &self.intr_pin {
            pin.deassert();
        }
    }

    /// Handle a read from the legacy virtio config BAR.
    fn bar_read(&self, offset: u16, len: usize) -> u32 {
        let mut vs = self.virtio_state.lock().expect("virtio lock poisoned");

        if offset >= bits::LEGACY_REG_DEVICE_CONFIG {
            // Device-specific config read
            let dev_offset = offset - bits::LEGACY_REG_DEVICE_CONFIG;
            return self.device.cfg_read(dev_offset, len as u8);
        }

        match offset {
            bits::LEGACY_REG_DEVICE_FEATURES => {
                self.device.device_features()
            }
            bits::LEGACY_REG_GUEST_FEATURES => {
                u32::from(vs.guest_features)
            }
            bits::LEGACY_REG_QUEUE_PFN => {
                vs.selected_queue()
                    .map(|q| q.pfn())
                    .unwrap_or(0)
            }
            bits::LEGACY_REG_QUEUE_SIZE => {
                vs.selected_queue()
                    .map(|q| u32::from(q.size()))
                    .unwrap_or(0)
            }
            bits::LEGACY_REG_QUEUE_SELECT => {
                u32::from(vs.queue_select)
            }
            bits::LEGACY_REG_QUEUE_NOTIFY => {
                // Write-only register, read returns 0
                0
            }
            bits::LEGACY_REG_DEVICE_STATUS => {
                u32::from(vs.status)
            }
            bits::LEGACY_REG_ISR_STATUS => {
                let val = vs.isr_status;
                if val != 0 {
                    vs.isr_status = 0;
                    drop(vs);
                    self.lower_interrupt();
                    return u32::from(val);
                }
                0
            }
            _ => 0,
        }
    }

    /// Handle a write to the legacy virtio config BAR.
    fn bar_write(&self, offset: u16, val: u32, len: usize) {
        let mut vs = self.virtio_state.lock().expect("virtio lock poisoned");

        if offset >= bits::LEGACY_REG_DEVICE_CONFIG {
            // Device-specific config write
            let dev_offset = offset - bits::LEGACY_REG_DEVICE_CONFIG;
            self.device.cfg_write(dev_offset, val, len as u8);
            return;
        }

        match offset {
            bits::LEGACY_REG_DEVICE_FEATURES => {
                // Read-only register, ignore writes
            }
            bits::LEGACY_REG_GUEST_FEATURES => {
                vs.guest_features = val;
                self.device.set_features(val);
            }
            bits::LEGACY_REG_QUEUE_PFN => {
                let queue_sel = vs.queue_select;
                if let Some(q) = vs.selected_queue_mut() {
                    q.set_addr_legacy(val);
                    // Notify the device of the new queue address
                    // (kernel-accelerated devices need to program
                    // their ring addresses at this point)
                    self.device.queue_addr_set(queue_sel, q);
                }
            }
            bits::LEGACY_REG_QUEUE_SELECT => {
                vs.queue_select = val as u16;
            }
            bits::LEGACY_REG_QUEUE_NOTIFY => {
                let queue_idx = val as u16;
                if vs.status & bits::STATUS_DRIVER_OK == 0 {
                    return;
                }
                let did_work = self.device.notify_queue(
                    queue_idx,
                    &mut vs.queues,
                    &self.physmap,
                );
                if did_work {
                    vs.isr_status |= bits::ISR_QUEUE_INTR;
                    drop(vs);
                    self.raise_interrupt();
                }
            }
            bits::LEGACY_REG_DEVICE_STATUS => {
                let new_status = val as u8;
                if new_status == 0 {
                    vs.reset();
                    self.device.reset();
                    drop(vs);
                    // Ensure interrupt line is low on reset so the
                    // next driver doesn't see a spurious assertion
                    // when it enables the IOAPIC pin.
                    self.lower_interrupt();
                } else {
                    vs.status = new_status;
                }
            }
            bits::LEGACY_REG_ISR_STATUS => {
                // Read-only register, ignore writes
            }
            _ => {}
        }
    }
}

impl<D: VirtioDevice> PciDevice for VirtioPciDevice<D> {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        self.pci_state
            .lock()
            .expect("pci lock poisoned")
            .cfg_read(offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        let mut pci = self.pci_state.lock().expect("pci lock poisoned");
        pci.cfg_write(offset, len, val);

        // After any config write that could affect BARs (BAR register
        // writes or Command register IO_EN changes), update the PIO
        // bus registration.
        let dword_off = offset & 0xFC;
        if dword_off == 0x04 || (0x10..=0x24).contains(&dword_off) {
            let weak = self.self_ref.lock().expect("self_ref lock");
            if let Some(arc_self) = weak.as_ref().and_then(|w| w.upgrade()) {
                drop(pci);
                arc_self.update_bar_registration(
                    &arc_self.pci_state.lock().expect("pci lock"),
                );
            }
        }
    }

    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        if bar != BarN::BAR0 {
            return;
        }

        match rwo {
            RWOp::Read(ro) => {
                let val = self.bar_read(offset as u16, ro.len());
                match ro.len() {
                    1 => ro.write_u8(val as u8),
                    2 => ro.write_u16(val as u16),
                    4 => ro.write_u32(val),
                    _ => ro.write_u32(val),
                }
            }
            RWOp::Write(wo) => {
                let val = match wo.len() {
                    1 => u32::from(wo.read_u8()),
                    2 => u32::from(wo.read_u16()),
                    4 => wo.read_u32(),
                    _ => wo.read_u32(),
                };
                self.bar_write(offset as u16, val, wo.len());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal test VirtIO device for transport tests.
    struct TestVirtioDevice {
        features: u32,
    }

    impl VirtioDevice for TestVirtioDevice {
        fn device_features(&self) -> u32 {
            self.features
        }

        fn set_features(&self, _features: u32) {}

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

    fn make_test_device() -> Arc<VirtioPciDevice<TestVirtioDevice>> {
        let physmap = Arc::new(PhysMap::new());
        let bus_pio = Arc::new(PioBus::new());
        VirtioPciDevice::new(
            TestVirtioDevice { features: 0x42 },
            bits::VIRTIO_DEV_TYPE_BLOCK,
            1,
            256,
            8,  // 8 bytes of device config
            None,
            physmap,
            bus_pio,
        )
    }

    #[test]
    fn pci_identity() {
        let dev = make_test_device();
        // Vendor ID
        let id = dev.cfg_read(0x00, 4);
        assert_eq!(id & 0xFFFF, bits::VIRTIO_PCI_VENDOR_ID as u32);
        // Device ID: 0x1000 + type - 1 = 0x1000 + 2 - 1 = 0x1001
        assert_eq!((id >> 16) & 0xFFFF, 0x1001);
    }

    #[test]
    fn pci_class() {
        let dev = make_test_device();
        let class_reg = dev.cfg_read(0x08, 4);
        let class = (class_reg >> 24) & 0xFF;
        assert_eq!(class, crate::pci::bits::CLASS_STORAGE as u32);
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
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
        ]);
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
        let wo = WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER]);
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
        {
            let mut vs = dev.virtio_state.lock().unwrap();
            vs.isr_status = bits::ISR_QUEUE_INTR;
        }

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
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
        ]);
        // TestVirtioDevice returns offset as value
        assert_eq!(val, 0);

        // At offset 0x18 (dev offset 4)
        let mut ro = ReadOp::new(4);
        dev.bar_rw(BarN::BAR0, 0x18, RWOp::Read(&mut ro));
        let val = u32::from_le_bytes([
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
        ]);
        assert_eq!(val, 4);
    }

    #[test]
    fn guest_features_write_read() {
        use vmm_core::common::{ReadOp, WriteOp};
        let dev = make_test_device();

        let wo = WriteOp::from_buf(&[0x01, 0x00, 0x00, 0x00]);
        dev.bar_rw(BarN::BAR0, 0x04, RWOp::Write(&wo));

        let mut ro = ReadOp::new(4);
        dev.bar_rw(BarN::BAR0, 0x04, RWOp::Read(&mut ro));
        let val = u32::from_le_bytes([
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
        ]);
        assert_eq!(val, 1);
    }
}
