// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO PCI transport (legacy + modern).
//!
//! Wraps a [`VirtioDevice`] and exposes it as a **transitional** PCI
//! device supporting both the legacy (pre-1.0) and modern (1.0+) PCI
//! transport mechanisms.
//!
//! # Transitional device layout
//!
//! - **BAR0** (PIO): Legacy virtio config registers
//! - **BAR2** (MMIO, 16 KB): Modern config structures (4 pages):
//!   - Page 0 (0x0000): Common configuration
//!   - Page 1 (0x1000): Device-specific configuration
//!   - Page 2 (0x2000): Notification register
//!   - Page 3 (0x3000): ISR status register
//! - **BAR4** (MMIO): MSI-X table + PBA (when MSI-X present)
//!
//! PCI capabilities advertise the modern config structures. A modern
//! guest negotiates `VIRTIO_F_VERSION_1` (bit 32) and uses BAR2.
//! Legacy guests use BAR0 as before. `EVENT_IDX` and `INDIRECT_DESC`
//! are only advertised through the modern path.

use std::sync::{Arc, Mutex};

use vmm_core::common::RWOp;
use vmm_core::mem::PhysMap;
use vmm_core::mmio::{MmioBus, MmioFn};
use vmm_core::pio::{PioBus, PioFn};

use crate::pci::bar::BarDefine;
use crate::pci::device::{DeviceIdent, DeviceState, PciDevice};
use crate::pci::msix::MsixTable;
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
    /// Features negotiated by the guest (u64 for spec compliance).
    guest_features: u64,
    /// Device-owned virtqueues.
    queues: Vec<VirtQueue>,
    /// MSI-X config vector (0xFFFF = no vector).
    config_msix_vector: u16,
    /// Per-queue MSI-X vectors (0xFFFF = no vector).
    queue_msix_vectors: Vec<u16>,

    // -- Modern transport state --

    /// Selects which 32-bit half of device features to read (0=low, 1=high).
    device_feature_select: u32,
    /// Selects which 32-bit half of driver features to write.
    driver_feature_select: u32,
    /// Per-queue enable flags (modern transport only).
    queue_enabled: Vec<bool>,
    /// Per-queue descriptor table addresses (modern transport).
    queue_desc: Vec<u64>,
    /// Per-queue available ring addresses (modern transport).
    queue_avail: Vec<u64>,
    /// Per-queue used ring addresses (modern transport).
    queue_used: Vec<u64>,
    /// Configuration generation counter.
    config_generation: u8,
}

impl VirtioPciState {
    fn new(num_queues: usize, queue_size: u16) -> Self {
        let queues = (0..num_queues)
            .map(|_| VirtQueue::new(queue_size))
            .collect();
        Self {
            queue_select: 0,
            status: 0,
            guest_features: 0,
            queues,
            config_msix_vector: bits::VIRTIO_MSI_NO_VECTOR,
            queue_msix_vectors: vec![bits::VIRTIO_MSI_NO_VECTOR; num_queues],
            device_feature_select: 0,
            driver_feature_select: 0,
            queue_enabled: vec![false; num_queues],
            queue_desc: vec![0; num_queues],
            queue_avail: vec![0; num_queues],
            queue_used: vec![0; num_queues],
            config_generation: 0,
        }
    }

    fn selected_queue(&self) -> Option<&VirtQueue> {
        self.queues.get(self.queue_select as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut VirtQueue> {
        self.queues.get_mut(self.queue_select as usize)
    }

    /// Return the low 32 bits of guest_features for legacy register reads.
    fn guest_features_u32(&self) -> u32 {
        self.guest_features as u32
    }

    fn reset(&mut self) {
        self.queue_select = 0;
        self.status = 0;
        self.guest_features = 0;
        self.config_msix_vector = bits::VIRTIO_MSI_NO_VECTOR;
        for v in &mut self.queue_msix_vectors {
            *v = bits::VIRTIO_MSI_NO_VECTOR;
        }
        for q in &mut self.queues {
            q.reset();
        }
        self.device_feature_select = 0;
        self.driver_feature_select = 0;
        for e in &mut self.queue_enabled {
            *e = false;
        }
        for a in &mut self.queue_desc {
            *a = 0;
        }
        for a in &mut self.queue_avail {
            *a = 0;
        }
        for a in &mut self.queue_used {
            *a = 0;
        }
    }

    /// Returns true if the guest negotiated VERSION_1 (modern transport).
    fn is_modern(&self) -> bool {
        self.guest_features & bits::VIRTIO_F_VERSION_1 != 0
    }
}

/// Offsets of virtio PCI capabilities in config space.
struct CapOffsets {
    /// First capability offset (either MSI-X or first virtio cap).
    first: u8,
    /// MSI-X capability offset (0 if no MSI-X).
    msix: u8,
    /// Common config capability offset.
    common: u8,
    /// Notify capability offset.
    notify: u8,
    /// ISR capability offset.
    isr: u8,
    /// Device config capability offset.
    device: u8,
}

impl CapOffsets {
    fn new(has_msix: bool) -> Self {
        if has_msix {
            // MSI-X cap (12 bytes) at 0x40, then virtio caps after
            Self {
                first: 0x40,
                msix: 0x40,
                common: 0x4C,
                notify: 0x5C,  // 0x4C + 16
                isr: 0x70,     // 0x5C + 20
                device: 0x80,  // 0x70 + 16
            }
        } else {
            Self {
                first: 0x40,
                msix: 0,
                common: 0x40,
                notify: 0x50,  // 0x40 + 16
                isr: 0x64,     // 0x50 + 20
                device: 0x74,  // 0x64 + 16
            }
        }
    }
}

/// PCI device wrapping a VirtIO backend.
///
/// Implements the [`PciDevice`] trait so it can be attached to the PCI
/// bus. Transitional device: BAR0 (PIO) for legacy, BAR2 (MMIO) for
/// modern transport.
pub struct VirtioPciDevice<D: VirtioDevice> {
    /// Standard PCI config header state.
    pci_state: Mutex<DeviceState>,
    /// VirtIO transport state.
    virtio_state: Mutex<VirtioPciState>,
    /// The backend device (e.g., block, net).
    device: D,
    /// ISR status — lock-free AtomicU8.
    /// Workers set via fetch_or, guest reads via swap(0).
    /// Eliminates Mutex contention between workers and vCPU.
    isr_status: std::sync::atomic::AtomicU8,
    /// Interrupt pin for legacy INTx delivery.
    intr_pin: Option<Arc<dyn IntrPin>>,
    /// MSI-X table (None = legacy INTx only).
    msix: Option<Arc<MsixTable>>,
    /// Lock-free per-queue MSI-X vector cache. Updated on guest writes
    /// to queue MSI-X vector registers; read by interrupt path without
    /// acquiring virtio_state (avoids deadlock when completion callbacks
    /// fire inside notify_queue while virtio_state is held).
    msix_queue_vectors: Vec<std::sync::atomic::AtomicU16>,
    /// Lock-free config MSI-X vector cache.
    msix_config_vector: std::sync::atomic::AtomicU16,
    /// Reference to guest physical memory for queue processing.
    physmap: Arc<PhysMap>,
    /// PIO bus for BAR0 registration/unregistration.
    bus_pio: Arc<PioBus>,
    /// MMIO bus for BAR2/BAR4 registration/unregistration.
    bus_mmio: Arc<MmioBus>,
    /// Currently registered BAR0 PIO base port.
    registered_bar: Mutex<Option<u16>>,
    /// Currently registered BAR2 MMIO base address.
    registered_bar2: Mutex<Option<u64>>,
    /// Currently registered BAR4 MMIO base address.
    registered_bar4: Mutex<Option<u64>>,
    /// Weak self-reference for BAR registration closures.
    self_ref: Mutex<Option<std::sync::Weak<Self>>>,
    /// Capability chain offsets in PCI config space.
    cap_offsets: CapOffsets,
    /// Size of device-specific config region (for cap advertisement).
    dev_config_size: u16,
    /// Number of queues (for modern config read).
    num_queues: u16,
}

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// Create a new VirtIO PCI device (transitional: legacy + modern).
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
    /// - `bus_pio` - PIO bus for BAR0
    /// - `bus_mmio` - MMIO bus for BAR2 (modern config) and BAR4 (MSI-X)
    /// - `msix` - Optional MSI-X table
    pub fn new(
        device: D,
        dev_type: u16,
        num_queues: usize,
        queue_size: u16,
        dev_config_size: u16,
        lintr_cfg: Option<LintrCfg>,
        physmap: Arc<PhysMap>,
        bus_pio: Arc<PioBus>,
        bus_mmio: Arc<MmioBus>,
        msix: Option<Arc<MsixTable>>,
    ) -> Arc<Self> {
        assert!(
            num_queues <= MAX_QUEUES,
            "too many queues (max {})",
            MAX_QUEUES
        );

        // Transitional device: PCI device ID 0x1000+type (legacy range)
        // with virtio PCI capabilities for modern discovery.
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

        // BAR0: PIO for legacy virtio config space
        let common_size = if msix.is_some() {
            bits::LEGACY_COMMON_SIZE_MSIX
        } else {
            bits::LEGACY_COMMON_SIZE
        };
        let bar_size = common_size + dev_config_size;
        let bar_size_aligned = bar_size.next_power_of_two().max(32);
        pci_state.define_bar(BarN::BAR0, BarDefine::Pio(bar_size_aligned));

        // BAR2: MMIO for modern transport config (16 KB, 4 pages)
        pci_state.define_bar(
            BarN::BAR2,
            BarDefine::Mmio(bits::MODERN_BAR_SIZE),
        );

        // BAR4: MMIO for MSI-X table + PBA
        if let Some(ref msix_table) = msix {
            let msix_bar_size = msix_table.bar_size() as u32;
            pci_state.define_bar(
                BarN::BAR4,
                BarDefine::Mmio(msix_bar_size.next_power_of_two()),
            );
        }

        // Capability chain: virtio PCI caps (+ MSI-X if present)
        let cap_offsets = CapOffsets::new(msix.is_some());
        pci_state.set_cap_ptr(cap_offsets.first);

        // Set interrupt pin and line register
        let intr_pin = if let Some((pin_id, pin)) = lintr_cfg {
            pci_state.set_intr_pin(pin_id as u8);
            pci_state.set_intr_line(10);
            Some(pin)
        } else {
            None
        };

        let msix_queue_vectors: Vec<std::sync::atomic::AtomicU16> = (0..num_queues)
            .map(|_| std::sync::atomic::AtomicU16::new(bits::VIRTIO_MSI_NO_VECTOR))
            .collect();

        let dev = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            virtio_state: Mutex::new(VirtioPciState::new(
                num_queues,
                queue_size,
            )),
            device,
            isr_status: std::sync::atomic::AtomicU8::new(0),
            intr_pin,
            msix,
            msix_queue_vectors,
            msix_config_vector: std::sync::atomic::AtomicU16::new(bits::VIRTIO_MSI_NO_VECTOR),
            physmap,
            bus_pio,
            bus_mmio,
            registered_bar: Mutex::new(None),
            registered_bar2: Mutex::new(None),
            registered_bar4: Mutex::new(None),
            self_ref: Mutex::new(None),
            cap_offsets,
            dev_config_size,
            num_queues: num_queues as u16,
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
            // best-effort: old handler may already be gone
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

    /// Update MMIO bus registration for BAR2 and BAR4.
    fn update_mmio_registration(self: &Arc<Self>, pci: &DeviceState) {
        let mmio_enabled =
            pci.command().contains(crate::pci::bits::RegCmd::MMIO_EN);

        // BAR2: modern transport config
        {
            let mut reg = self.registered_bar2.lock().expect("bar2 lock");
            if let Some(old_addr) = reg.take() {
                let _ = self.bus_mmio.unregister(old_addr);
            }
            if mmio_enabled {
                if let Some((def, addr)) = pci.bars().get(BarN::BAR2) {
                    if addr != 0 && def.is_mmio() {
                        let size = def.size();
                        let dev = Arc::clone(self);
                        let handler: Arc<MmioFn> =
                            Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                                dev.bar_rw(BarN::BAR2, offset, rwo);
                            });
                        if self.bus_mmio.register(addr, size, handler).is_ok() {
                            *reg = Some(addr);
                        }
                    }
                }
            }
        }

        // BAR4: MSI-X table + PBA
        if self.msix.is_some() {
            let mut reg = self.registered_bar4.lock().expect("bar4 lock");
            if let Some(old_addr) = reg.take() {
                let _ = self.bus_mmio.unregister(old_addr);
            }
            if mmio_enabled {
                if let Some((def, addr)) = pci.bars().get(BarN::BAR4) {
                    if addr != 0 && def.is_mmio() {
                        let size = def.size();
                        let dev = Arc::clone(self);
                        let handler: Arc<MmioFn> =
                            Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                                dev.bar_rw(BarN::BAR4, offset, rwo);
                            });
                        if self.bus_mmio.register(addr, size, handler).is_ok() {
                            *reg = Some(addr);
                        }
                    }
                }
            }
        }
    }

    /// Snapshot PCI BAR addresses and command bits for migration.
    fn export_migrate_pci_state(
        &self,
    ) -> crate::lifecycle::VirtioMigratePciState {
        let pci = self.pci_state.lock().expect("pci lock");
        let mut bar_addrs = [0u64; 6];
        for (idx, addr) in bar_addrs.iter_mut().enumerate() {
            let bar = BarN::try_from(idx as u8).expect("valid BAR index");
            *addr = pci.bars().get(bar).map(|(_, value)| value).unwrap_or(0);
        }

        crate::lifecycle::VirtioMigratePciState {
            command: pci.command().bits(),
            bar_addrs,
        }
    }

    /// Replay BAR and command-register writes so bus registrations are restored.
    fn restore_migrate_pci_state(
        &self,
        state: &crate::lifecycle::VirtioMigratePciState,
    ) {
        let mut bar_writes = Vec::with_capacity(7);
        {
            let pci = self.pci_state.lock().expect("pci lock");
            for (idx, addr) in state.bar_addrs.iter().copied().enumerate() {
                let bar = BarN::try_from(idx as u8).expect("valid BAR index");
                let Some((def, _)) = pci.bars().get(bar) else {
                    continue;
                };

                let offset = crate::pci::bits::REG_BAR0 + (idx as u8 * 4);
                match def {
                    BarDefine::Pio(_) | BarDefine::Mmio(_) => {
                        bar_writes.push((offset, addr as u32));
                    }
                    BarDefine::Mmio64(_) => {
                        bar_writes.push((offset, addr as u32));
                        bar_writes.push((offset + 4, (addr >> 32) as u32));
                    }
                }
            }
        }

        for (offset, value) in bar_writes {
            self.cfg_write(offset, 4, value);
        }
        self.cfg_write(
            crate::pci::bits::REG_COMMAND,
            2,
            u32::from(state.command),
        );
    }

    // -- Modern transport: features --

    /// Features advertised through the modern transport path.
    ///
    /// Device features already include EVENT_IDX and INDIRECT_DESC
    /// for devices that support them (block, rng). Viona strips
    /// EVENT_IDX since it handles rings in-kernel.
    fn modern_features(&self) -> u64 {
        self.device.device_features() | bits::VIRTIO_F_VERSION_1
    }

    // -- PCI capability chain read/write --

    /// Read a dword from a virtio PCI capability structure.
    ///
    /// Returns `Some(val)` if `offset` falls within a virtio or MSI-X
    /// capability, `None` otherwise.
    fn cap_chain_read(&self, offset: u8) -> Option<u32> {
        let co = &self.cap_offsets;

        // MSI-X capability (12 bytes)
        if co.msix != 0 && offset >= co.msix && offset < co.msix + 12 {
            if let Some(ref msix) = self.msix {
                let mut val = msix.cap_read(offset, co.msix)?;
                // Patch next-pointer: MSI-X byte 1 → first virtio cap
                if offset == co.msix {
                    val = (val & !0x0000_FF00) | (u32::from(co.common) << 8);
                }
                return Some(val);
            }
        }

        // Virtio common config cap (16 bytes)
        if offset >= co.common && offset < co.common + 16 {
            return Some(self.read_virtio_cap(
                offset - co.common,
                bits::VIRTIO_PCI_CAP_COMMON_CFG,
                co.notify,
                bits::MODERN_BAR_IDX,
                bits::MODERN_BAR_COMMON_OFFSET,
                bits::COMMON_CFG_SIZE as u32,
            ));
        }

        // Virtio notify cap (20 bytes)
        if offset >= co.notify && offset < co.notify + 20 {
            let rel = offset - co.notify;
            if rel >= 16 {
                // Extra dword: notify_off_multiplier = 0
                return Some(0);
            }
            return Some(self.read_virtio_cap(
                rel,
                bits::VIRTIO_PCI_CAP_NOTIFY_CFG,
                co.isr,
                bits::MODERN_BAR_IDX,
                bits::MODERN_BAR_NOTIFY_OFFSET,
                4,
            ));
        }

        // Virtio ISR cap (16 bytes)
        if offset >= co.isr && offset < co.isr + 16 {
            return Some(self.read_virtio_cap(
                offset - co.isr,
                bits::VIRTIO_PCI_CAP_ISR_CFG,
                co.device,
                bits::MODERN_BAR_IDX,
                bits::MODERN_BAR_ISR_OFFSET,
                4,
            ));
        }

        // Virtio device config cap (16 bytes)
        if offset >= co.device && offset < co.device + 16 {
            return Some(self.read_virtio_cap(
                offset - co.device,
                bits::VIRTIO_PCI_CAP_DEVICE_CFG,
                0, // last in chain
                bits::MODERN_BAR_IDX,
                bits::MODERN_BAR_DEVICE_OFFSET,
                u32::from(self.dev_config_size),
            ));
        }

        None
    }

    /// Build a dword from a virtio_pci_cap structure at relative offset.
    fn read_virtio_cap(
        &self,
        rel: u8,
        cfg_type: u8,
        next: u8,
        bar: u8,
        offset: u32,
        length: u32,
    ) -> u32 {
        let dword_off = rel & 0xFC;
        match dword_off {
            // Byte 0: cap_vndr=0x09, Byte 1: cap_next, Byte 2: cap_len, Byte 3: cfg_type
            0 => {
                let cap_len = if cfg_type == bits::VIRTIO_PCI_CAP_NOTIFY_CFG {
                    bits::VIRTIO_PCI_NOTIFY_CAP_SIZE
                } else {
                    bits::VIRTIO_PCI_CAP_SIZE
                };
                u32::from(bits::PCI_CAP_ID_VNDR)
                    | (u32::from(next) << 8)
                    | (u32::from(cap_len) << 16)
                    | (u32::from(cfg_type) << 24)
            }
            // Byte 4: bar, Byte 5: id(0), Bytes 6-7: padding
            4 => u32::from(bar),
            // Bytes 8-11: offset within BAR
            8 => offset,
            // Bytes 12-15: length
            12 => length,
            _ => 0,
        }
    }

    /// Handle writes to virtio PCI capabilities (mostly read-only).
    fn cap_chain_write(&self, offset: u8, val: u32) {
        let co = &self.cap_offsets;
        // Only MSI-X has writable fields
        if co.msix != 0 && offset >= co.msix && offset < co.msix + 12 {
            if let Some(ref msix) = self.msix {
                msix.cap_write(offset, co.msix, val);
            }
        }
        // All virtio cap fields are read-only
    }

    // -- Modern transport: BAR2 MMIO handlers --

    /// Handle a read from the modern config BAR (BAR2).
    fn modern_bar_read(&self, offset: u16, len: usize) -> u32 {
        let page = offset & 0xF000;
        let reg = offset & 0x0FFF;

        match page {
            0x0000 => self.modern_common_read(reg, len),
            0x1000 => self.device.cfg_read(reg, len as u8),
            0x2000 => 0, // notify is write-only
            0x3000 => {
                // ISR status (same semantics as legacy: read clears)
                let val = self.isr_status.swap(
                    0,
                    std::sync::atomic::Ordering::AcqRel,
                );
                if val != 0 {
                    self.lower_interrupt();
                    if self.isr_status.load(
                        std::sync::atomic::Ordering::Acquire,
                    ) != 0 {
                        self.raise_interrupt();
                    }
                }
                u32::from(val)
            }
            _ => 0,
        }
    }

    /// Handle a write to the modern config BAR (BAR2).
    fn modern_bar_write(&self, offset: u16, val: u32, len: usize) {
        let page = offset & 0xF000;
        let reg = offset & 0x0FFF;

        match page {
            0x0000 => self.modern_common_write(reg, val, len),
            0x1000 => self.device.cfg_write(reg, val, len as u8),
            0x2000 => {
                // Notification register: guest writes queue index
                let queue_idx = val as u16;
                let mut vs = self.virtio_state.lock().expect("virtio lock");
                if vs.status & bits::STATUS_DRIVER_OK == 0 {
                    return;
                }
                let did_work = self.device.notify_queue(
                    queue_idx,
                    &mut vs.queues,
                    &self.physmap,
                );
                if did_work {
                    self.isr_status.fetch_or(
                        bits::ISR_QUEUE_INTR,
                        std::sync::atomic::Ordering::Release,
                    );
                    drop(vs);
                    self.raise_queue_interrupt_for(queue_idx);
                }
            }
            0x3000 => {} // ISR is read-only
            _ => {}
        }
    }

    /// Read from the modern common configuration structure.
    fn modern_common_read(&self, offset: u16, _len: usize) -> u32 {
        let vs = self.virtio_state.lock().expect("virtio lock");

        match offset {
            bits::COMMON_CFG_DEVICE_FEATURE_SELECT => vs.device_feature_select,
            bits::COMMON_CFG_DEVICE_FEATURE => {
                let feats = self.modern_features();
                match vs.device_feature_select {
                    0 => feats as u32,
                    1 => (feats >> 32) as u32,
                    _ => 0,
                }
            }
            bits::COMMON_CFG_DRIVER_FEATURE_SELECT => vs.driver_feature_select,
            bits::COMMON_CFG_DRIVER_FEATURE => {
                match vs.driver_feature_select {
                    0 => vs.guest_features as u32,
                    1 => (vs.guest_features >> 32) as u32,
                    _ => 0,
                }
            }
            bits::COMMON_CFG_MSIX_CONFIG => u32::from(vs.config_msix_vector),
            bits::COMMON_CFG_NUM_QUEUES => u32::from(self.num_queues),
            bits::COMMON_CFG_DEVICE_STATUS => u32::from(vs.status),
            bits::COMMON_CFG_CONFIG_GENERATION => u32::from(vs.config_generation),
            bits::COMMON_CFG_QUEUE_SELECT => u32::from(vs.queue_select),
            bits::COMMON_CFG_QUEUE_SIZE => {
                vs.selected_queue()
                    .map(|q| u32::from(q.size()))
                    .unwrap_or(0)
            }
            bits::COMMON_CFG_QUEUE_MSIX_VECTOR => {
                let idx = vs.queue_select as usize;
                u32::from(
                    vs.queue_msix_vectors
                        .get(idx)
                        .copied()
                        .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR),
                )
            }
            bits::COMMON_CFG_QUEUE_ENABLE => {
                let idx = vs.queue_select as usize;
                u32::from(
                    vs.queue_enabled.get(idx).copied().unwrap_or(false) as u16,
                )
            }
            bits::COMMON_CFG_QUEUE_NOTIFY_OFF => {
                // With multiplier=0, all queues notify at the same offset
                u32::from(vs.queue_select)
            }
            bits::COMMON_CFG_QUEUE_DESC_LO => {
                let idx = vs.queue_select as usize;
                vs.queue_desc.get(idx).copied().unwrap_or(0) as u32
            }
            bits::COMMON_CFG_QUEUE_DESC_HI => {
                let idx = vs.queue_select as usize;
                (vs.queue_desc.get(idx).copied().unwrap_or(0) >> 32) as u32
            }
            bits::COMMON_CFG_QUEUE_AVAIL_LO => {
                let idx = vs.queue_select as usize;
                vs.queue_avail.get(idx).copied().unwrap_or(0) as u32
            }
            bits::COMMON_CFG_QUEUE_AVAIL_HI => {
                let idx = vs.queue_select as usize;
                (vs.queue_avail.get(idx).copied().unwrap_or(0) >> 32) as u32
            }
            bits::COMMON_CFG_QUEUE_USED_LO => {
                let idx = vs.queue_select as usize;
                vs.queue_used.get(idx).copied().unwrap_or(0) as u32
            }
            bits::COMMON_CFG_QUEUE_USED_HI => {
                let idx = vs.queue_select as usize;
                (vs.queue_used.get(idx).copied().unwrap_or(0) >> 32) as u32
            }
            _ => 0,
        }
    }

    /// Write to the modern common configuration structure.
    fn modern_common_write(&self, offset: u16, val: u32, _len: usize) {
        let mut vs = self.virtio_state.lock().expect("virtio lock");

        match offset {
            bits::COMMON_CFG_DEVICE_FEATURE_SELECT => {
                vs.device_feature_select = val;
            }
            bits::COMMON_CFG_DRIVER_FEATURE_SELECT => {
                vs.driver_feature_select = val;
            }
            bits::COMMON_CFG_DRIVER_FEATURE => {
                // Write the selected 32-bit half of driver features.
                let offered = self.modern_features();
                match vs.driver_feature_select {
                    0 => {
                        let accepted = val & (offered as u32);
                        vs.guest_features =
                            (vs.guest_features & 0xFFFF_FFFF_0000_0000)
                            | u64::from(accepted);
                    }
                    1 => {
                        let accepted = val & ((offered >> 32) as u32);
                        vs.guest_features =
                            (vs.guest_features & 0x0000_0000_FFFF_FFFF)
                            | (u64::from(accepted) << 32);
                    }
                    _ => {}
                }
                // Propagate to device and queues
                let features = vs.guest_features;
                self.device.set_features(features);
                let event_idx =
                    features & bits::VIRTIO_F_RING_EVENT_IDX != 0;
                let indirect =
                    features & bits::VIRTIO_F_RING_INDIRECT_DESC != 0;
                for q in &mut vs.queues {
                    q.set_event_idx(event_idx);
                    q.set_indirect_supported(indirect);
                }
            }
            bits::COMMON_CFG_MSIX_CONFIG => {
                vs.config_msix_vector = val as u16;
                self.msix_config_vector.store(val as u16, std::sync::atomic::Ordering::Release);
            }
            bits::COMMON_CFG_DEVICE_STATUS => {
                let new_status = val as u8;
                if new_status == 0 {
                    vs.reset();
                    self.device.reset();
                    self.isr_status.store(
                        0,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    // Clear atomic MSI-X vector cache
                    self.msix_config_vector.store(bits::VIRTIO_MSI_NO_VECTOR, std::sync::atomic::Ordering::Relaxed);
                    for a in &self.msix_queue_vectors {
                        a.store(bits::VIRTIO_MSI_NO_VECTOR, std::sync::atomic::Ordering::Relaxed);
                    }
                    drop(vs);
                    self.lower_interrupt();
                } else {
                    if new_status & bits::STATUS_FEATURES_OK != 0
                        && vs.status & bits::STATUS_FEATURES_OK == 0
                    {
                        vs.status = new_status;
                    } else {
                        vs.status = new_status;
                    }
                }
            }
            bits::COMMON_CFG_QUEUE_SELECT => {
                vs.queue_select = val as u16;
            }
            bits::COMMON_CFG_QUEUE_SIZE => {
                // Modern transport allows writing queue size (power of 2).
                // We accept only if it equals our fixed size (simplification).
            }
            bits::COMMON_CFG_QUEUE_MSIX_VECTOR => {
                let idx = vs.queue_select as usize;
                if idx < vs.queue_msix_vectors.len() {
                    vs.queue_msix_vectors[idx] = val as u16;
                    if let Some(a) = self.msix_queue_vectors.get(idx) {
                        a.store(val as u16, std::sync::atomic::Ordering::Release);
                    }
                }
            }
            bits::COMMON_CFG_QUEUE_ENABLE => {
                let idx = vs.queue_select as usize;
                if idx < vs.queue_enabled.len() && val == 1 {
                    // Enable the queue: apply the stored addresses
                    vs.queue_enabled[idx] = true;
                    let desc = vs.queue_desc[idx];
                    let avail = vs.queue_avail[idx];
                    let used = vs.queue_used[idx];
                    if let Some(q) = vs.queues.get_mut(idx) {
                        q.set_addr_modern(desc, avail, used);
                        self.device.queue_addr_set(idx as u16, q);
                    }
                }
            }
            bits::COMMON_CFG_QUEUE_DESC_LO => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_desc.get_mut(idx) {
                    *a = (*a & 0xFFFF_FFFF_0000_0000) | u64::from(val);
                }
            }
            bits::COMMON_CFG_QUEUE_DESC_HI => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_desc.get_mut(idx) {
                    *a = (*a & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32);
                }
            }
            bits::COMMON_CFG_QUEUE_AVAIL_LO => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_avail.get_mut(idx) {
                    *a = (*a & 0xFFFF_FFFF_0000_0000) | u64::from(val);
                }
            }
            bits::COMMON_CFG_QUEUE_AVAIL_HI => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_avail.get_mut(idx) {
                    *a = (*a & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32);
                }
            }
            bits::COMMON_CFG_QUEUE_USED_LO => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_used.get_mut(idx) {
                    *a = (*a & 0xFFFF_FFFF_0000_0000) | u64::from(val);
                }
            }
            bits::COMMON_CFG_QUEUE_USED_HI => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_used.get_mut(idx) {
                    *a = (*a & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32);
                }
            }
            _ => {}
        }
    }

    /// Assert the legacy interrupt to notify the guest.
    ///
    /// Uses edge-triggered delivery: deassert then reassert to generate
    /// a new edge. The PciIntrPin calls `vm_isa_assert_irq(atpic, ioapic)`
    /// which delivers through both the legacy PIC and IOAPIC paths.
    ///
    /// Assert the PCI interrupt pin (level-triggered).
    ///
    /// PCI INTx is level-triggered: assert when work is pending,
    /// deassert when the guest reads ISR (clears the pending flag).
    /// No deassert here — that prevents the spurious IRQ race where
    /// deassert+assert creates a second edge that the guest sees
    /// with ISR=0, causing "nobody cared" → IRQ disable.
    pub fn raise_interrupt(&self) {
        if let Some(pin) = &self.intr_pin {
            pin.assert();
        }
    }

    /// Raise a queue interrupt: set ISR status and assert the pin.
    ///
    /// Uses AtomicU8 for ISR to avoid locking virtio_state on the
    /// hot path. Workers and vCPU can set ISR concurrently without
    /// Mutex contention.
    pub fn raise_queue_interrupt(&self) {
        self.isr_status.fetch_or(bits::ISR_QUEUE_INTR, std::sync::atomic::Ordering::Release);
        self.raise_interrupt();
    }

    /// Deassert the interrupt after ISR is read or on device reset.
    fn lower_interrupt(&self) {
        if let Some(pin) = &self.intr_pin {
            pin.deassert();
        }
    }

    /// Return the BAR0 offset where device-specific config begins.
    ///
    /// With MSI-X, two extra 16-bit vector registers are inserted
    /// between ISR and device config (at 0x14 and 0x16), pushing
    /// device config to 0x18.
    fn device_config_offset(&self) -> u16 {
        if self.msix.is_some() {
            bits::LEGACY_REG_DEVICE_CONFIG_MSIX
        } else {
            bits::LEGACY_REG_DEVICE_CONFIG
        }
    }

    /// Raise an interrupt for a specific queue, routing via MSI-X
    /// when available or falling back to legacy INTx.
    pub fn raise_queue_interrupt_for(&self, queue_idx: u16) {
        if let Some(ref msix) = self.msix {
            // Read vector from lock-free atomic cache (no virtio_state lock
            // needed — safe to call from inside notify_queue callbacks).
            let vector = self.msix_queue_vectors
                .get(queue_idx as usize)
                .map(|a| a.load(std::sync::atomic::Ordering::Acquire))
                .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR);
            if vector != bits::VIRTIO_MSI_NO_VECTOR {
                msix.fire(vector);
                return;
            }
            eprintln!("[virtio-intr] q={} vector=NO_VECTOR, falling back to INTx", queue_idx);
        } else {
            eprintln!("[virtio-intr] q={} no msix, using INTx", queue_idx);
        }
        // Fallback to legacy INTx
        self.raise_queue_interrupt();
    }

    /// Signal a device configuration change to the guest.
    #[allow(dead_code)]
    pub fn signal_config_change(&self) {
        if let Some(ref msix) = self.msix {
            let vector = self.msix_config_vector.load(std::sync::atomic::Ordering::Acquire);
            if vector != bits::VIRTIO_MSI_NO_VECTOR {
                msix.fire(vector);
                return;
            }
        }
        self.isr_status.fetch_or(
            bits::ISR_CFG_CHANGE,
            std::sync::atomic::Ordering::Release,
        );
        self.raise_interrupt();
    }

    /// Signal the guest that the device needs a reset.
    #[allow(dead_code)]
    pub fn signal_device_needs_reset(&self) {
        let mut vs = self.virtio_state.lock().expect("virtio lock poisoned");
        vs.status |= bits::STATUS_DEVICE_NEEDS_RESET;
        drop(vs);
        self.isr_status.fetch_or(
            bits::ISR_CFG_CHANGE,
            std::sync::atomic::Ordering::Release,
        );
        self.raise_interrupt();
    }

    /// Handle a read from the legacy virtio config BAR.
    fn bar_read(&self, offset: u16, len: usize) -> u32 {
        let vs = self.virtio_state.lock().expect("virtio lock poisoned");

        let dev_cfg_off = self.device_config_offset();
        if offset >= dev_cfg_off {
            // Device-specific config read
            let dev_offset = offset - dev_cfg_off;
            return self.device.cfg_read(dev_offset, len as u8);
        }

        match offset {
            bits::LEGACY_REG_DEVICE_FEATURES => {
                // Truncate to 32-bit for legacy transport.
                // Strip EVENT_IDX and INDIRECT_DESC: these cause
                // deadlock with legacy transport on Linux 6.14.
                // They are available via the modern BAR2 path.
                (self.device.device_features() as u32)
                    & !bits::VIRTIO_F_RING_EVENT_IDX_U32
                    & !bits::VIRTIO_F_RING_INDIRECT_DESC_U32
            }
            bits::LEGACY_REG_GUEST_FEATURES => {
                vs.guest_features_u32()
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
                // Atomic swap: read and clear ISR in one operation.
                let val = self.isr_status.swap(0, std::sync::atomic::Ordering::AcqRel);
                if val != 0 {
                    drop(vs);
                    self.lower_interrupt();
                    // Re-check: a worker may have set ISR between our
                    // swap and deassert. If so, re-assert the pin so
                    // the guest gets another interrupt for the new work.
                    if self.isr_status.load(std::sync::atomic::Ordering::Acquire) != 0 {
                        self.raise_interrupt();
                    }
                    return u32::from(val);
                }
                0
            }
            bits::LEGACY_REG_MSIX_CONFIG_VECTOR if self.msix.is_some() => {
                u32::from(vs.config_msix_vector)
            }
            bits::LEGACY_REG_MSIX_QUEUE_VECTOR if self.msix.is_some() => {
                let idx = vs.queue_select as usize;
                u32::from(
                    vs.queue_msix_vectors
                        .get(idx)
                        .copied()
                        .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR),
                )
            }
            _ => 0,
        }
    }

    /// Handle a write to the legacy virtio config BAR.
    fn bar_write(&self, offset: u16, val: u32, len: usize) {
        let mut vs = self.virtio_state.lock().expect("virtio lock poisoned");

        let dev_cfg_off = self.device_config_offset();
        if offset >= dev_cfg_off {
            // Device-specific config write
            let dev_offset = offset - dev_cfg_off;
            self.device.cfg_write(dev_offset, val, len as u8);
            return;
        }

        match offset {
            bits::LEGACY_REG_DEVICE_FEATURES => {
                // Read-only register, ignore writes
            }
            bits::LEGACY_REG_GUEST_FEATURES => {
                // Mask: only accept features the device offered via legacy.
                // Strip EVENT_IDX/INDIRECT_DESC from legacy offered set.
                let offered = (self.device.device_features() as u32)
                    & !bits::VIRTIO_F_RING_EVENT_IDX_U32
                    & !bits::VIRTIO_F_RING_INDIRECT_DESC_U32;
                let accepted = val & offered;
                vs.guest_features = u64::from(accepted);
                self.device.set_features(u64::from(accepted));

                // Propagate event_idx and indirect_supported to all
                // queues so they know the negotiated capabilities.
                let accepted64 = vs.guest_features;
                let event_idx = accepted64 & bits::VIRTIO_F_RING_EVENT_IDX != 0;
                let indirect = accepted64 & bits::VIRTIO_F_RING_INDIRECT_DESC != 0;
                for q in &mut vs.queues {
                    q.set_event_idx(event_idx);
                    q.set_indirect_supported(indirect);
                }
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
                    drop(vs);
                    self.raise_queue_interrupt_for(queue_idx);
                }
            }
            bits::LEGACY_REG_DEVICE_STATUS => {
                let new_status = val as u8;
                if new_status == 0 {
                    vs.reset();
                    self.device.reset();
                    self.isr_status.store(0, std::sync::atomic::Ordering::Relaxed);
                    self.msix_config_vector.store(bits::VIRTIO_MSI_NO_VECTOR, std::sync::atomic::Ordering::Relaxed);
                    for a in &self.msix_queue_vectors {
                        a.store(bits::VIRTIO_MSI_NO_VECTOR, std::sync::atomic::Ordering::Relaxed);
                    }
                    drop(vs);
                    // Ensure interrupt line is low on reset so the
                    // next driver doesn't see a spurious assertion
                    // when it enables the IOAPIC pin.
                    self.lower_interrupt();
                } else {
                    // FEATURES_OK validation (VirtIO 1.3 sec 2.2):
                    // If the driver sets FEATURES_OK, verify the
                    // negotiated feature subset is acceptable. If
                    // not, clear FEATURES_OK so the driver sees the
                    // rejection on re-read.
                    if new_status & bits::STATUS_FEATURES_OK != 0
                        && vs.status & bits::STATUS_FEATURES_OK == 0
                    {
                        // Accept all subsets for now — reject only if
                        // a future device requires mandatory features.
                        vs.status = new_status;
                    } else {
                        vs.status = new_status;
                    }
                }
            }
            bits::LEGACY_REG_ISR_STATUS => {
                // Read-only register, ignore writes
            }
            bits::LEGACY_REG_MSIX_CONFIG_VECTOR if self.msix.is_some() => {
                vs.config_msix_vector = val as u16;
                self.msix_config_vector.store(val as u16, std::sync::atomic::Ordering::Release);
            }
            bits::LEGACY_REG_MSIX_QUEUE_VECTOR if self.msix.is_some() => {
                let idx = vs.queue_select as usize;
                if idx < vs.queue_msix_vectors.len() {
                    vs.queue_msix_vectors[idx] = val as u16;
                    if let Some(a) = self.msix_queue_vectors.get(idx) {
                        a.store(val as u16, std::sync::atomic::Ordering::Release);
                    }
                }
            }
            _ => {}
        }
    }
}

impl<D: VirtioDevice> PciDevice for VirtioPciDevice<D> {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        // Check capability chain (virtio caps + MSI-X) first.
        // cap_chain_read works on dword-aligned offsets, so align
        // and extract the requested bytes afterwards.
        if offset >= 0x40 {
            let dword_off = offset & 0xFC;
            let byte_off = (offset & 0x03) as u32;
            if let Some(dword_val) = self.cap_chain_read(dword_off) {
                let mask = match len {
                    1 => 0xFF,
                    2 => 0xFFFF,
                    _ => 0xFFFF_FFFF,
                };
                return (dword_val >> (byte_off * 8)) & mask;
            }
        }
        self.pci_state
            .lock()
            .expect("pci lock poisoned")
            .cfg_read(offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        // Handle capability chain writes (MSI-X has writable fields).
        // Sub-dword writes need read-modify-write: read the current
        // capability dword, merge the written bytes, then pass the
        // full dword so cap_write sees fields at their correct bit
        // positions (e.g., Message Control at bits 31:16).
        if offset >= 0x40 {
            let dword_off = offset & 0xFC;
            let byte_in_dword = (offset & 0x03) as u32;
            let dword_val = if len < 4 {
                let cur = self.cap_chain_read(dword_off).unwrap_or(0);
                let shift = byte_in_dword * 8;
                let mask = match len {
                    1 => 0xFFu32 << shift,
                    2 => 0xFFFFu32 << shift,
                    _ => 0xFFFF_FFFFu32,
                };
                (cur & !mask) | ((val << shift) & mask)
            } else {
                val
            };
            self.cap_chain_write(dword_off, dword_val);
        }
        let mut pci = self.pci_state.lock().expect("pci lock poisoned");
        pci.cfg_write(offset, len, val);

        // After any config write that could affect BARs or command
        // register, update PIO and MMIO bus registrations.
        let dword_off = offset & 0xFC;
        if dword_off == 0x04 || (0x10..=0x24).contains(&dword_off) {
            let weak = self.self_ref.lock().expect("self_ref lock");
            if let Some(arc_self) = weak.as_ref().and_then(|w| w.upgrade()) {
                drop(pci);
                let pci_ref = arc_self.pci_state.lock().expect("pci lock");
                arc_self.update_bar_registration(&pci_ref);
                arc_self.update_mmio_registration(&pci_ref);
            }
        }
    }

    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        // BAR4: MSI-X table + PBA access
        if bar == BarN::BAR4 {
            if let Some(ref msix) = self.msix {
                let pba_offset = msix.pba_offset();
                match rwo {
                    RWOp::Read(ro) => {
                        let val = if offset >= pba_offset {
                            msix.pba_read(offset - pba_offset)
                        } else {
                            msix.table_read(offset)
                        };
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
                        if offset < pba_offset {
                            msix.table_write(offset, val);
                        }
                    }
                }
            }
            return;
        }

        // BAR2: Modern transport config (MMIO)
        if bar == BarN::BAR2 {
            match rwo {
                RWOp::Read(ro) => {
                    let val = self.modern_bar_read(offset as u16, ro.len());
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
                    self.modern_bar_write(offset as u16, val, wo.len());
                }
            }
            return;
        }

        // BAR0: Legacy transport config (PIO)
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

// ---------------------------------------------------------------------------
// Lifecycle delegation — forward VM lifecycle calls to the inner device.
// ---------------------------------------------------------------------------

use crate::Lifecycle;
use crate::migrate::Migrator;
impl<D: VirtioDevice + Lifecycle> Lifecycle for VirtioPciDevice<D> {
    fn type_name(&self) -> &'static str {
        Lifecycle::type_name(&self.device)
    }

    fn lifecycle_state(&self) -> Option<crate::lifecycle::IndicatedState> {
        Lifecycle::lifecycle_state(&self.device)
    }

    fn start(&self) -> anyhow::Result<()> {
        Lifecycle::start(&self.device)
    }

    fn pause(&self) {
        Lifecycle::pause(&self.device)
    }

    fn is_quiesced(&self) -> bool {
        Lifecycle::is_quiesced(&self.device)
    }

    fn resume(&self) {
        Lifecycle::resume(&self.device)
    }

    fn reset(&self) {
        Lifecycle::reset(&self.device)
    }

    fn halt(&self) {
        Lifecycle::halt(&self.device)
    }

    fn pause_for_migration(&self) {
        self.device.pause_rings_for_export();
    }

    fn flush_backing(
        &self,
        intent: crate::FlushIntent,
    ) -> Result<(), crate::FlushError> {
        Lifecycle::flush_backing(&self.device, intent)
    }

    fn post_restore_kick(&self) {
        // Re-inject interrupts on all enabled queues after vCPUs are
        // running. This ensures the guest's NAPI handler runs and
        // replenishes rx buffers.
        let vs = self.virtio_state.lock().expect("virtio lock");
        let enabled: Vec<u16> = (0..self.num_queues)
            .filter(|&idx| vs.queue_enabled.get(idx as usize).copied().unwrap_or(false))
            .collect();
        drop(vs);
        for idx in enabled {
            self.raise_queue_interrupt_for(idx);
        }
    }

    fn migrate(&'_ self) -> Migrator<'_> {
        Lifecycle::migrate(&self.device)
    }

    fn export_virtio_state(&self) -> Vec<crate::lifecycle::VirtioMigrateQueue> {
        use crate::lifecycle::VirtioMigrateQueue;
        // Ring threads are already paused by pause_for_migration()
        // which runs BEFORE vCPU pause. This ensures the guest's
        // descriptor state is consistent with the kernel's indices.
        let vs = self.virtio_state.lock().expect("virtio lock");
        let pci_state = self.export_migrate_pci_state();
        let mut result = Vec::new();
        for idx in 0..self.num_queues as usize {
            let q = &vs.queues[idx];
            if q.is_configured() {
                let (k_avail, k_used) = self.device.get_kernel_ring_indices(idx as u16);
                // For software devices (blk), read used_idx from guest
                // memory because VirtioCompletion manages the used ring
                // separately from VirtQueue's shadow_used_idx.
                let used_idx = if k_used != 0 {
                    k_used
                } else {
                    let guest_used = q.read_used_ring_idx(&self.physmap);
                    if guest_used != 0 { guest_used } else { q.shadow_used_idx() }
                };
                // For kernel-managed queues (viona), use the kernel's
                // actual consumed position (k_avail). The delta between
                // k_avail and guest memory's avail_ring->idx represents
                // entries the guest posted but the kernel hasn't consumed.
                // On the dest, these entries are immediately available —
                // their descriptors are valid (guest set them up, kernel
                // hasn't touched them). Using guest memory's avail_ring->idx
                // instead would make the dest kernel see 0 available
                // entries, requiring complex workarounds.
                let avail_idx = if k_used != 0 {
                    k_avail
                } else {
                    q.last_avail_idx()
                };

                // Get MSI-X entry for this queue's vector
                let vector = self.msix_queue_vectors
                    .get(idx)
                    .map(|a| a.load(std::sync::atomic::Ordering::Acquire))
                    .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR);
                let (msix_addr, msix_data) = if let Some(ref msix) = self.msix {
                    if vector != bits::VIRTIO_MSI_NO_VECTOR {
                        msix.read_entry(vector)
                    } else { (0, 0) }
                } else { (0, 0) };

                result.push(VirtioMigrateQueue {
                    queue_idx: idx as u16,
                    queue_size: q.size(),
                    desc_addr: q.desc_addr(),
                    avail_addr: q.avail_addr(),
                    used_addr: q.used_addr(),
                    enabled: vs.queue_enabled.get(idx).copied().unwrap_or(false),
                    features: vs.guest_features,
                    avail_idx,
                    used_idx,
                    msix_addr,
                    msix_data: msix_data as u32,
                    msix_vector: vector,
                    pci_state: Some(pci_state.clone()),
                });
                // Detailed ring state dump for migration debugging
                let guest_avail = q.read_avail_idx(&self.physmap);
                let guest_used = q.read_used_ring_idx(&self.physmap);
                eprintln!("[virtio-export] q={} k_avail={} k_used={} guest_avail={} guest_used={} export_avail={} export_used={}",
                    idx, k_avail, k_used, guest_avail, guest_used, avail_idx, used_idx);
                if k_used != 0 && k_avail == guest_avail {
                    // Fully drained — dump avail ring entries to see descriptor IDs
                    let qsize = q.size();
                    let mut ids = Vec::new();
                    for slot in 0..qsize.min(16) {
                        let gpa = q.avail_addr() + 4 + u64::from(slot) * 2;
                        if let Some(id) = self.physmap.lookup(gpa, 2)
                            .and_then(|s| s.read::<u16>().ok()) {
                            ids.push(id);
                        }
                    }
                    eprintln!("[virtio-export] q={} DRAINED avail_ring[0..15]={:?}", idx, ids);
                }
            }
        }
        result
    }

    fn restore_virtio_state(&self, states: &[crate::lifecycle::VirtioMigrateQueue]) {
        // Group by queue_idx=0 starts, find group matching our queue count
        let my_nq = self.num_queues as usize;
        let matching: Vec<_> = {
            let mut groups: Vec<Vec<_>> = Vec::new();
            let mut cur: Vec<_> = Vec::new();
            for s in states {
                if s.queue_idx == 0 && !cur.is_empty() {
                    groups.push(std::mem::take(&mut cur));
                }
                cur.push(s.clone());
            }
            if !cur.is_empty() { groups.push(cur); }
            groups.into_iter().find(|g| g.len() == my_nq).unwrap_or_default()
        };

        if let Some(pci_state) = matching.iter().find_map(|qs| qs.pci_state.as_ref()) {
            eprintln!("[virtio-restore] restoring PCI: cmd={:#x} bars={:?}", pci_state.command, pci_state.bar_addrs);
            self.restore_migrate_pci_state(pci_state);
        } else {
            eprintln!("[virtio-restore] NO pci_state in matching queues (nq={})", my_nq);
        }

        let mut vs = self.virtio_state.lock().expect("virtio lock");
        self.device.reset_all_rings();

        for qs in &matching {
            let idx = qs.queue_idx as usize;
            if idx >= vs.queues.len() { continue; }

            if vs.guest_features == 0 && qs.features != 0 {
                vs.guest_features = qs.features;
                self.device.set_features(qs.features);
                // Propagate ring feature flags to all queues — the
                // normal guest write path does this in the feature
                // register handler, but restore bypasses it. Without
                // this, indirect_supported stays false and
                // collect_chain() silently drops every indirect
                // descriptor request (all Linux virtio-blk I/O).
                let event_idx =
                    qs.features & bits::VIRTIO_F_RING_EVENT_IDX != 0;
                let indirect =
                    qs.features & bits::VIRTIO_F_RING_INDIRECT_DESC != 0;
                for q in &mut vs.queues {
                    q.set_event_idx(event_idx);
                    q.set_indirect_supported(indirect);
                }
            }

            vs.queues[idx].set_addr_modern(qs.desc_addr, qs.avail_addr, qs.used_addr);
            // Restore ring indices for software-emulated devices.
            // set_addr_modern resets indices to 0, but after migration
            // we need to continue from the source's position.
            vs.queues[idx].set_last_avail_idx(qs.avail_idx);
            vs.queues[idx].set_shadow_used_idx(qs.used_idx);
            eprintln!("[virtio-restore] q={} avail_idx={} used_idx={} addrs={:#x}/{:#x}/{:#x}",
                qs.queue_idx, qs.avail_idx, qs.used_idx,
                qs.desc_addr, qs.avail_addr, qs.used_addr);
            if idx < vs.queue_desc.len() {
                vs.queue_desc[idx] = qs.desc_addr;
                vs.queue_avail[idx] = qs.avail_addr;
                vs.queue_used[idx] = qs.used_addr;
            }
            if idx < vs.queue_enabled.len() {
                vs.queue_enabled[idx] = qs.enabled;
            }

            // Restore MSI-X vector mapping for this queue
            if qs.msix_vector != bits::VIRTIO_MSI_NO_VECTOR {
                if idx < vs.queue_msix_vectors.len() {
                    vs.queue_msix_vectors[idx] = qs.msix_vector;
                }
                if let Some(a) = self.msix_queue_vectors.get(idx) {
                    a.store(qs.msix_vector, std::sync::atomic::Ordering::Release);
                }
                // Write the MSI-X table entry so fire() can inject the interrupt
                if let Some(ref msix) = self.msix {
                    msix.write_entry(qs.msix_vector, qs.msix_addr, qs.msix_data as u64);
                }
            }

            if qs.enabled {
                // Log ring state for debugging
                let guest_avail_gpa = qs.avail_addr + 2;
                let guest_used_gpa = qs.used_addr + 2;
                let ga = self.physmap.lookup(guest_avail_gpa, 2)
                    .and_then(|s| s.read::<u16>().ok()).unwrap_or(0);
                let gu = self.physmap.lookup(guest_used_gpa, 2)
                    .and_then(|s| s.read::<u16>().ok()).unwrap_or(0);
                let avail_delta = ga.wrapping_sub(qs.avail_idx);
                eprintln!(
                    "[virtio-restore] q={} kernel(avail={} used={}) guest_mem(avail={} used={}) available={}",
                    qs.queue_idx, qs.avail_idx, qs.used_idx, ga, gu, avail_delta,
                );

                self.device.restore_ring_state(
                    qs.queue_idx, qs.queue_size,
                    qs.desc_addr, qs.avail_addr, qs.used_addr,
                    qs.avail_idx, qs.used_idx,
                    qs.msix_addr, qs.msix_data,
                );
            }
        }

        if !matching.is_empty() {
            vs.status = bits::STATUS_DRIVER_OK
                | bits::STATUS_FEATURES_OK
                | bits::STATUS_DRIVER
                | bits::STATUS_ACKNOWLEDGE;

            // Enable MSI-X so fire() can deliver interrupts
            if let Some(ref msix) = self.msix {
                msix.set_enabled(true);
            }
        }

        // Program notification addresses from current BAR state
        let pci = self.pci_state.lock().expect("pci lock");
        let bar0_port = pci.bars().get(BarN::BAR0)
            .and_then(|(def, addr)| if def.is_pio() { Some(addr as u16) } else { None })
            .unwrap_or(0);
        let bar2_addr = pci.bars().get(BarN::BAR2)
            .and_then(|(def, addr)| if def.is_mmio() { Some(addr) } else { None })
            .unwrap_or(0);
        drop(pci);
        eprintln!("[virtio-restore] BAR0={:#x} BAR2={:#x}", bar0_port, bar2_addr);

        if bar0_port != 0 || bar2_addr != 0 {
            let notify_pio = if bar0_port != 0 { bar0_port + bits::LEGACY_REG_QUEUE_NOTIFY } else { 0 };
            let notify_mmio = if bar2_addr != 0 { bar2_addr + bits::MODERN_BAR_NOTIFY_OFFSET as u64 } else { 0 };
            self.device.set_notify_addrs(notify_pio, notify_mmio);
        }

        // Start the deferred interrupt poll thread now that rings,
        // MSI-X vectors, and notification addresses are all programmed.
        self.device.start_poll_deferred();

        // Inject a post-restore interrupt on each queue to wake the
        // guest driver. After migration, the rx ring may have no
        // available buffers (avail_idx == used_idx). The guest driver
        // needs an interrupt to process the "caught up" used ring and
        // post new rx buffers. Without this, incoming packets are
        // dropped with "no_space" in the kernel viona driver.
        drop(vs);
        for qs in &matching {
            if qs.enabled {
                eprintln!("[virtio-restore] injecting post-restore interrupt q={}", qs.queue_idx);
                self.raise_queue_interrupt_for(qs.queue_idx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
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

    impl crate::Lifecycle for TestVirtioDevice {
        fn type_name(&self) -> &'static str {
            "test-virtio"
        }
    }

    fn make_test_device() -> Arc<VirtioPciDevice<TestVirtioDevice>> {
        let physmap = Arc::new(PhysMap::new());
        let bus_pio = Arc::new(PioBus::new());
        let bus_mmio = Arc::new(MmioBus::new());
        VirtioPciDevice::new(
            TestVirtioDevice {
                features: 0x42
                    | bits::VIRTIO_F_RING_EVENT_IDX
                    | bits::VIRTIO_F_RING_INDIRECT_DESC,
            },
            bits::VIRTIO_DEV_TYPE_BLOCK,
            1,
            256,
            8,  // 8 bytes of device config
            None,
            physmap,
            bus_pio,
            bus_mmio,
            None,  // no MSI-X
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

    impl crate::Lifecycle for TrackingVirtioDevice {
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
        dev.isr_status.store(bits::ISR_QUEUE_INTR, std::sync::atomic::Ordering::Relaxed);

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

        // Device offers 0x42; write 0x42 so masking yields 0x42.
        let wo = WriteOp::from_buf(&[0x42, 0x00, 0x00, 0x00]);
        dev.bar_rw(BarN::BAR0, 0x04, RWOp::Write(&wo));

        let mut ro = ReadOp::new(4);
        dev.bar_rw(BarN::BAR0, 0x04, RWOp::Read(&mut ro));
        let val = u32::from_le_bytes([
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
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
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
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
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
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
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
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

        let wo = WriteOp::from_buf(&[
            bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER,
        ]);
        dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

        let wo = WriteOp::from_buf(&[
            bits::STATUS_ACKNOWLEDGE
                | bits::STATUS_DRIVER
                | bits::STATUS_FEATURES_OK,
        ]);
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
        dev.isr_status.store(
            bits::ISR_CFG_CHANGE,
            std::sync::atomic::Ordering::Relaxed,
        );

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
        assert_eq!(
            ro.buf()[0],
            bits::ISR_QUEUE_INTR | bits::ISR_CFG_CHANGE,
        );
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
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
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

        let command = crate::pci::bits::RegCmd::IO_EN
            | crate::pci::bits::RegCmd::MMIO_EN
            | crate::pci::bits::RegCmd::BUSMSTR_EN
            | crate::pci::bits::RegCmd::INTX_DIS;
        dev.cfg_write(crate::pci::bits::REG_BAR0, 4, 0xC120);
        dev.cfg_write(crate::pci::bits::REG_BAR2, 4, 0xFEC0_0000);
        dev.cfg_write(crate::pci::bits::REG_COMMAND, 2, u32::from(command.bits()));

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
        let command = crate::pci::bits::RegCmd::IO_EN
            | crate::pci::bits::RegCmd::MMIO_EN
            | crate::pci::bits::RegCmd::BUSMSTR_EN
            | crate::pci::bits::RegCmd::INTX_DIS;
        let queue_state = crate::lifecycle::VirtioMigrateQueue {
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
            pci_state: Some(crate::lifecycle::VirtioMigratePciState {
                command: command.bits(),
                bar_addrs: [0xC120, 0, 0xFEC0_0000, 0, 0, 0],
            }),
        };

        dev.restore_virtio_state(&[queue_state]);

        assert_eq!(dev.cfg_read(crate::pci::bits::REG_BAR0, 4), 0xC121);
        assert_eq!(dev.cfg_read(crate::pci::bits::REG_BAR2, 4), 0xFEC0_0000);
        assert_eq!(
            dev.cfg_read(crate::pci::bits::REG_COMMAND, 2),
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
        let queue_state = crate::lifecycle::VirtioMigrateQueue {
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
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
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
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
        ]);
        // EVENT_IDX = bit 29
        assert_ne!(
            low & (1 << 29),
            0,
            "EVENT_IDX should be in modern features"
        );
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
            ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3],
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
}
