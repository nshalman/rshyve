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

use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{DeviceIdent, DeviceState, PciDevice};
use vmm_devices::pci::msix::MsixTable;
use vmm_devices::pci::{BarN, IntrPin, LintrCfg};

use super::bits;
use super::queue::VirtQueue;
use super::VirtioDevice;

pub mod intr;
mod migrate;
mod modern;

use intr::IntrGate;

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

    /// Set while a backend reset drains its guest-memory accesses.
    ///
    /// The transport lock is released across that drain so a register
    /// read is still served, which leaves the device reachable to
    /// another vCPU while it is half reset. The latch sits inside the
    /// locked state so a write cannot test it and then wait for the
    /// lock: every write path tests the latch and changes the
    /// registers in one critical section, and is dropped when the
    /// latch is closed.
    resetting: bool,
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
            resetting: false,
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
        self.queue_msix_vectors.fill(bits::VIRTIO_MSI_NO_VECTOR);
        for q in &mut self.queues {
            q.reset();
        }
        self.device_feature_select = 0;
        self.driver_feature_select = 0;
        self.queue_enabled.fill(false);
        self.queue_desc.fill(0);
        self.queue_avail.fill(0);
        self.queue_used.fill(0);
        // `resetting` stays as it is. The reset path owns the latch
        // and opens it only once every field above is published.
    }

    /// The device status the driver may read.
    ///
    /// The vCPU that asked for the reset is inside it, so this read
    /// comes from another one. A driver frees the vring as soon as it
    /// reads 0 (VirtIO 1.3 sec 4.1.4.3.1) and writes ACKNOWLEDGE next,
    /// and neither is safe while the backend drains. A reset that
    /// starts from 0, which is what a driver does when it probes a
    /// device it never used, would otherwise report 0 for the whole
    /// drain.
    fn readable_status(&self) -> u8 {
        if self.resetting {
            self.status | bits::STATUS_ACKNOWLEDGE
        } else {
            self.status
        }
    }

    /// Returns true if the guest negotiated VERSION_1 (modern transport).
    // Retained for the pending transport-mode validation path.
    #[allow(dead_code)]
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
                notify: 0x5C, // 0x4C + 16
                isr: 0x70,    // 0x5C + 20
                device: 0x80, // 0x70 + 16
            }
        } else {
            Self {
                first: 0x40,
                msix: 0,
                common: 0x40,
                notify: 0x50, // 0x40 + 16
                isr: 0x64,    // 0x50 + 20
                device: 0x74, // 0x64 + 16
            }
        }
    }
}

/// One-shot park points a test installs to hold a thread at a named
/// step of the reset protocol. The first thread to reach a point takes
/// the hook and runs it, so a test can pin an interleaving that would
/// otherwise depend on the scheduler.
#[cfg(test)]
#[derive(Default)]
struct TestParks {
    /// Reached in a register write path once its reset-latch check has
    /// passed, and before the write changes any transport state.
    write_checked: ParkSlot,
    /// Reached once the transport lock is dropped and before the
    /// config interrupt is delivered, which is the window a reset can
    /// run in.
    config_interrupt_pending: ParkSlot,
    /// Reached between sampling the session for an MSI-X unmask and
    /// delivering what the unmask released, which is the window a
    /// whole reset can run in.
    released_delivery_pending: ParkSlot,
}

#[cfg(test)]
type ParkSlot = Mutex<Option<Arc<dyn Fn() + Send + Sync>>>;

/// Run a park point, if a test installed one, and clear it.
#[cfg(test)]
fn run_park(slot: &ParkSlot) {
    let hook = slot.lock().expect("park lock poisoned").take();
    if let Some(hook) = hook {
        hook();
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
    /// Structured logger for bus registration diagnostics.
    log: slog::Logger,
    /// Set once a refused queue has been reported at `warn`. The write
    /// that refuses is guest driven and repeatable, so the rest go to
    /// `debug`.
    queue_refusal_reported: std::sync::atomic::AtomicBool,
    /// Orders every interrupt this device delivers against its own
    /// reset.
    intr: Arc<IntrGate>,
    /// Park points a test uses to pin a reset interleaving.
    #[cfg(test)]
    parks: TestParks,
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
    ///
    /// Bus registration failures go to a discarding logger. Use
    /// [`new_with_logger`](Self::new_with_logger) to see them.
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
        Self::new_with_logger(
            device,
            dev_type,
            num_queues,
            queue_size,
            dev_config_size,
            lintr_cfg,
            physmap,
            bus_pio,
            bus_mmio,
            msix,
            slog::Logger::root(slog::Discard, slog::o!()),
        )
    }

    /// Create a new VirtIO PCI device with a logger for bus
    /// registration diagnostics.
    pub fn new_with_logger(
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
        log: slog::Logger,
    ) -> Arc<Self> {
        assert!(
            num_queues <= MAX_QUEUES,
            "too many queues (max {})",
            MAX_QUEUES
        );

        // Transitional device: legacy-range PCI device id, plus virtio PCI
        // capabilities for modern discovery.
        let pci_device_id = bits::transitional_device_id(dev_type);
        let (pci_class, pci_subclass) = bits::pci_class_for(dev_type);

        let ident = DeviceIdent {
            vendor_id: bits::VIRTIO_PCI_VENDOR_ID,
            device_id: pci_device_id,
            class: pci_class,
            subclass: pci_subclass,
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
        pci_state
            .define_bar(BarN::BAR2, BarDefine::Mmio(bits::MODERN_BAR_SIZE));

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

        let msix_queue_vectors: Vec<std::sync::atomic::AtomicU16> = (0
            ..num_queues)
            .map(|_| {
                std::sync::atomic::AtomicU16::new(bits::VIRTIO_MSI_NO_VECTOR)
            })
            .collect();

        let dev = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            virtio_state: Mutex::new(VirtioPciState::new(
                num_queues, queue_size,
            )),
            device,
            isr_status: std::sync::atomic::AtomicU8::new(0),
            intr_pin,
            msix,
            msix_queue_vectors,
            msix_config_vector: std::sync::atomic::AtomicU16::new(
                bits::VIRTIO_MSI_NO_VECTOR,
            ),
            physmap,
            bus_pio,
            bus_mmio,
            registered_bar: Mutex::new(None),
            registered_bar2: Mutex::new(None),
            registered_bar4: Mutex::new(None),
            self_ref: Mutex::new(None),
            log,
            queue_refusal_reported: std::sync::atomic::AtomicBool::new(false),
            intr: Arc::new(IntrGate::new()),
            #[cfg(test)]
            parks: TestParks::default(),
            cap_offsets,
            dev_config_size,
            num_queues: num_queues as u16,
        });
        // Store weak self-reference for BAR registration
        *dev.self_ref.lock().expect("self_ref lock") =
            Some(Arc::downgrade(&dev));
        dev
    }

    /// Unregister the recorded BAR0 PIO base and clear the record.
    fn release_pio(&self, reg: &mut Option<u16>) {
        if let Some(port) = reg.take() {
            if let Err(e) = self.bus_pio.unregister(port) {
                slog::warn!(self.log, "virtio-pci: PIO unregister failed";
                    "bar" => ?BarN::BAR0,
                    "port" => format!("{port:#x}"),
                    "error" => %e);
            }
        }
    }

    /// Unregister a recorded MMIO base and clear the record.
    fn release_mmio(&self, bar: BarN, reg: &mut Option<u64>) {
        if let Some(addr) = reg.take() {
            if let Err(e) = self.bus_mmio.unregister(addr) {
                slog::warn!(self.log, "virtio-pci: MMIO unregister failed";
                    "bar" => ?bar,
                    "addr" => format!("{addr:#x}"),
                    "error" => %e);
            }
        }
    }

    /// Update PIO bus registration for BAR0 based on current state.
    ///
    /// Must be called with an Arc<Self> available. Registers or
    /// unregisters the PIO handler as needed.
    fn update_bar_registration(self: &Arc<Self>, pci: &DeviceState) {
        let io_enabled = pci
            .command()
            .contains(vmm_devices::pci::bits::RegCmd::IO_EN);
        let bar_info = pci.bars().get(BarN::BAR0);

        let mut reg = self.registered_bar.lock().expect("bar lock poisoned");
        self.release_pio(&mut reg);

        // Register new handler if IO is enabled and BAR has an address
        if io_enabled {
            if let Some((BarDefine::Pio(size), addr)) = bar_info {
                let port = addr as u16;
                if port != 0 {
                    let dev = Arc::clone(self);
                    let handler: Arc<PioFn> =
                        Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                            dev.bar_rw(BarN::BAR0, offset as usize, rwo);
                        });
                    match self.bus_pio.register(port, size, handler) {
                        Ok(()) => *reg = Some(port),
                        // The device now decodes nothing on BAR0.
                        Err(e) => {
                            slog::error!(self.log,
                                "virtio-pci: PIO register failed, BAR0 dark";
                                "bar" => ?BarN::BAR0,
                                "port" => format!("{port:#x}"),
                                "size" => size,
                                "error" => %e);
                        }
                    }
                }
            }
        }
    }

    /// Update MMIO bus registration for BAR2 and BAR4.
    fn update_mmio_registration(self: &Arc<Self>, pci: &DeviceState) {
        let mmio_enabled = pci
            .command()
            .contains(vmm_devices::pci::bits::RegCmd::MMIO_EN);

        // BAR2: modern transport config
        {
            let mut reg = self.registered_bar2.lock().expect("bar2 lock");
            self.release_mmio(BarN::BAR2, &mut reg);
            if mmio_enabled {
                self.register_mmio(BarN::BAR2, pci, &mut reg);
            }
        }

        // BAR4: MSI-X table + PBA
        if self.msix.is_some() {
            let mut reg = self.registered_bar4.lock().expect("bar4 lock");
            self.release_mmio(BarN::BAR4, &mut reg);
            if mmio_enabled {
                self.register_mmio(BarN::BAR4, pci, &mut reg);
            }
        }
    }

    /// Register one MMIO BAR and record its base on success.
    fn register_mmio(
        self: &Arc<Self>,
        bar: BarN,
        pci: &DeviceState,
        reg: &mut Option<u64>,
    ) {
        let Some((def, addr)) = pci.bars().get(bar) else {
            return;
        };
        if addr == 0 || !def.is_mmio() {
            return;
        }
        let size = def.size();
        let dev = Arc::clone(self);
        let handler: Arc<MmioFn> =
            Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                dev.bar_rw(bar, offset, rwo);
            });
        match self.bus_mmio.register(addr, size, handler) {
            Ok(()) => *reg = Some(addr),
            // The device now decodes nothing on this BAR.
            Err(e) => {
                slog::error!(self.log,
                    "virtio-pci: MMIO register failed, BAR dark";
                    "bar" => ?bar,
                    "addr" => format!("{addr:#x}"),
                    "size" => size,
                    "error" => %e);
            }
        }
    }

    /// Take the transport lock, unless a reset is draining behind it.
    ///
    /// Every guest write goes through here. The latch is part of the
    /// locked state, so the caller cannot read it and then wait for
    /// the lock: a write either runs before the reset closes the
    /// latch or is dropped, never in between.
    fn lock_for_write(
        &self,
    ) -> Option<std::sync::MutexGuard<'_, VirtioPciState>> {
        let vs = self.virtio_state.lock().expect("virtio lock poisoned");
        if vs.resetting {
            return None;
        }
        #[cfg(test)]
        run_park(&self.parks.write_checked);
        Some(vs)
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

    /// Whether a queue refused the ring addresses on its first use.
    ///
    /// A queue programmed outside the enable path, such as by a
    /// migration restore, is only checked when the device first reads
    /// the ring. It can do no work from then on, so the driver has to
    /// be told rather than left waiting on it.
    fn queue_needs_reset(vs: &VirtioPciState, queue_idx: u16) -> bool {
        vs.queues
            .get(queue_idx as usize)
            .is_some_and(|q| q.needs_reset())
    }

    /// Refuse a queue the guest just programmed and tell it why.
    ///
    /// The status bit goes in under the guard that refused the queue,
    /// and the guard is then consumed so the caller cannot hold it
    /// across the interrupt. Nothing happens once the device already
    /// carries DEVICE_NEEDS_RESET: the write that reaches here is
    /// guest driven and can be repeated without limit, so the
    /// interrupt must not be raised per write. The reason is reported
    /// once per device for the same reason: a guest can reset and
    /// reprogram in a loop, and the second report says nothing the
    /// first did not.
    fn refuse_queue(
        &self,
        mut vs: std::sync::MutexGuard<'_, VirtioPciState>,
        idx: usize,
        error: Option<crate::queue::QueueAddrError>,
    ) {
        if vs.status & bits::STATUS_DEVICE_NEEDS_RESET != 0 {
            return;
        }
        // The bit must go in here. Applied after the guard drops, it
        // can land in the middle of a reset drain, or on the session
        // that follows the reset.
        vs.status |= bits::STATUS_DEVICE_NEEDS_RESET;
        // Sampled under the guard that wrote the bit, so a reset that
        // wipes it is always the later of the two.
        let session = self.intr.session();
        drop(vs);
        if let Some(error) = error {
            let first = !self
                .queue_refusal_reported
                .swap(true, std::sync::atomic::Ordering::Relaxed);
            if first {
                slog::warn!(self.log, "virtio-pci refused a queue";
                    "queue" => idx, "reason" => %error);
            } else {
                slog::debug!(self.log, "virtio-pci refused a queue";
                    "queue" => idx, "reason" => %error);
            }
        }
        self.raise_config_interrupt_in(session);
    }

    /// Install the one-shot park a register write reaches once its
    /// reset-latch check has passed.
    #[cfg(test)]
    fn park_write_after_latch_check(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.parks.write_checked.lock().expect("park lock poisoned") =
            Some(hook);
    }

    /// Install the one-shot park an MSI-X unmask reaches once it has
    /// taken the message out of the pending array.
    #[cfg(test)]
    fn park_before_released_delivery(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .parks
            .released_delivery_pending
            .lock()
            .expect("park lock poisoned") = Some(hook);
    }

    /// Install the one-shot park a config interrupt reaches once its
    /// caller has dropped the transport lock.
    #[cfg(test)]
    fn park_before_config_interrupt(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .parks
            .config_interrupt_pending
            .lock()
            .expect("park lock poisoned") = Some(hook);
    }

    /// Reset the device, on the vCPU that wrote DEVICE_STATUS.
    ///
    /// A legacy driver writes the register once and does not poll.
    /// illumos `virtio_legacy_device_reset_locked()` is that one
    /// write, and `virtio_shutdown()` reclaims the ring DMA straight
    /// after it, so the write must not return while the backend can
    /// still reach guest memory. A modern driver polls for 0 (VirtIO
    /// 1.3 sec 4.1.4.3.1) and the same order satisfies it.
    ///
    /// The wait is short by construction. A backing-store operation
    /// owns only host memory, so the backend reset ends the request
    /// epoch and waits out the guest-memory accesses that hold it,
    /// never the I/O behind them.
    ///
    /// The latch closes first because the transport lock is dropped
    /// across the backend reset. While it is closed every register
    /// write is dropped and `readable_status` reports non-zero, so a
    /// second vCPU cannot act on a device that is half reset.
    ///
    /// The caller holds the transport lock, so the latch closes in the
    /// same critical section that tested it.
    fn reset_device(&self, mut vs: std::sync::MutexGuard<'_, VirtioPciState>) {
        if vs.resetting {
            return;
        }
        vs.resetting = true;
        vs.status &= !bits::STATUS_DRIVER_OK;
        vs.queue_enabled.fill(false);
        // The driver session ends here, and interrupt admission shuts
        // with it. An interrupt an earlier event asked for must not
        // reach the driver that comes next. Admission stays shut for
        // the whole of `finish_reset`, so nothing joins the set that
        // reset has to wait out.
        self.intr.end_session();
        drop(vs);

        self.finish_reset();
    }

    /// Finish the reset the guest asked for, and publish the result.
    ///
    /// The backend wait has no deadline. It covers guest-memory
    /// accesses only, so it is bounded by construction, and a device
    /// that gave up on one could not take back the write a worker had
    /// already started into the guest chain.
    fn finish_reset(&self) {
        self.device.reset();

        // Every delivery this session admitted has finished. Nothing
        // joins them: admission shut before the backend reset. The
        // wait is over transport work only, one interrupt injection
        // per thread already inside, and a guest cannot add threads.
        self.intr.settle();

        // The next driver must find this device off the line, or it
        // sees a spurious assertion when it enables the IOAPIC pin.
        // The ISR and the pin move together, so a driver on another
        // vCPU cannot read the ISR between them.
        {
            let _line = self.intr.line();
            self.isr_status
                .store(0, std::sync::atomic::Ordering::Relaxed);
            self.lower_interrupt();
        }
        // Messages the old session left pending would otherwise leave
        // on the next unmask, which is a guest write this reset does
        // not gate.
        if let Some(ref msix) = self.msix {
            msix.clear_pending();
        }

        let mut vs = self.virtio_state.lock().expect("virtio lock poisoned");
        vs.reset();
        // Clear atomic MSI-X vector cache
        self.msix_config_vector.store(
            bits::VIRTIO_MSI_NO_VECTOR,
            std::sync::atomic::Ordering::Relaxed,
        );
        for a in &self.msix_queue_vectors {
            a.store(
                bits::VIRTIO_MSI_NO_VECTOR,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        // Status 0 and the open latch reach the guest together. The
        // driver writes ACKNOWLEDGE next, and that write must not meet
        // a latch that is still closed. Interrupt admission reopens in
        // the same critical section, so no register write can get
        // through before deliveries are being admitted again.
        vs.resetting = false;
        self.intr.reopen();
        drop(vs);
    }

    /// Signal the guest that the device needs a reset.
    ///
    /// A reset in flight takes the device down anyway, so the bit is
    /// dropped rather than written onto the session that follows.
    pub fn signal_device_needs_reset(&self) {
        let Some(mut vs) = self.lock_for_write() else {
            return;
        };
        vs.status |= bits::STATUS_DEVICE_NEEDS_RESET;
        let session = self.intr.session();
        drop(vs);
        self.raise_config_interrupt_in(session);
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
            bits::LEGACY_REG_GUEST_FEATURES => vs.guest_features_u32(),
            bits::LEGACY_REG_QUEUE_PFN => {
                vs.selected_queue().map(|q| q.pfn()).unwrap_or(0)
            }
            bits::LEGACY_REG_QUEUE_SIZE => vs
                .selected_queue()
                .map(|q| u32::from(q.size()))
                .unwrap_or(0),
            bits::LEGACY_REG_QUEUE_SELECT => u32::from(vs.queue_select),
            bits::LEGACY_REG_QUEUE_NOTIFY => {
                // Write-only register, read returns 0
                0
            }
            bits::LEGACY_REG_DEVICE_STATUS => u32::from(vs.readable_status()),
            bits::LEGACY_REG_ISR_STATUS => {
                // The line guard is taken with the transport lock
                // already given up: a raise holds the line guard and
                // an ISR read must not be able to hold the transport
                // lock behind it.
                drop(vs);
                u32::from(self.read_isr())
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
        let Some(mut vs) = self.lock_for_write() else {
            return;
        };

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
                let indirect =
                    accepted64 & bits::VIRTIO_F_RING_INDIRECT_DESC != 0;
                for q in &mut vs.queues {
                    q.set_event_idx(event_idx);
                    q.set_indirect_supported(indirect);
                }
            }
            bits::LEGACY_REG_QUEUE_PFN => {
                let queue_sel = vs.queue_select;
                let idx = queue_sel as usize;
                let log = self.log.clone();
                // A zero PFN is how a legacy driver takes the ring
                // down, so only a ring the guest is programming is
                // checked.
                let verdict = vs.selected_queue_mut().map(|q| {
                    q.set_log(log);
                    q.set_addr_legacy(val);
                    if val == 0 {
                        Ok(())
                    } else {
                        q.validate_addrs(&self.physmap)
                    }
                });
                match verdict {
                    None => {}
                    Some(Err(error)) => {
                        if let Some(q) = vs.selected_queue_mut() {
                            q.reset();
                        }
                        self.refuse_queue(vs, idx, Some(error));
                    }
                    Some(Ok(())) => {
                        // Notify the device of the new queue address
                        // (kernel-accelerated devices need to program
                        // their ring addresses at this point)
                        if let Some(q) = vs.queues.get(idx) {
                            self.device.queue_addr_set(queue_sel, q);
                        }
                    }
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
                if Self::queue_needs_reset(&vs, queue_idx) {
                    // The queue logged its own reason when it refused.
                    self.refuse_queue(vs, queue_idx as usize, None);
                    return;
                }
                if did_work {
                    // Sampled under the guard, so a reset that ends
                    // this session is always the later of the two.
                    let session = self.intr.session();
                    drop(vs);
                    self.raise_queue_interrupt_in(session, queue_idx);
                }
            }
            bits::LEGACY_REG_DEVICE_STATUS => {
                let new_status = val as u8;
                if new_status == 0 {
                    self.reset_device(vs);
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
                self.msix_config_vector
                    .store(val as u16, std::sync::atomic::Ordering::Release);
            }
            bits::LEGACY_REG_MSIX_QUEUE_VECTOR if self.msix.is_some() => {
                let idx = vs.queue_select as usize;
                if idx < vs.queue_msix_vectors.len() {
                    vs.queue_msix_vectors[idx] = val as u16;
                    if let Some(a) = self.msix_queue_vectors.get(idx) {
                        a.store(
                            val as u16,
                            std::sync::atomic::Ordering::Release,
                        );
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
                            // Sampled before the write takes the
                            // message out of the pending array.
                            let session = self.intr.session();
                            let released =
                                msix.table_write_deferred(offset, val);
                            self.deliver_released(session, msix, released);
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

    fn detach_regions(&self) {
        {
            let mut reg =
                self.registered_bar.lock().expect("bar lock poisoned");
            self.release_pio(&mut reg);
        }
        {
            let mut reg = self.registered_bar2.lock().expect("bar2 lock");
            self.release_mmio(BarN::BAR2, &mut reg);
        }
        {
            let mut reg = self.registered_bar4.lock().expect("bar4 lock");
            self.release_mmio(BarN::BAR4, &mut reg);
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle delegation — forward VM lifecycle calls to the inner device.
// ---------------------------------------------------------------------------

use vmm_devices::migrate::Migrator;
use vmm_devices::Lifecycle;
impl<D: VirtioDevice + Lifecycle> Lifecycle for VirtioPciDevice<D> {
    fn type_name(&self) -> &'static str {
        Lifecycle::type_name(&self.device)
    }

    fn lifecycle_state(
        &self,
    ) -> Option<vmm_devices::lifecycle::IndicatedState> {
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
        Lifecycle::halt(&self.device);
    }

    fn pause_for_migration(&self) {
        self.device.pause_rings_for_export();
    }

    fn flush_backing(
        &self,
        intent: vmm_devices::FlushIntent,
    ) -> Result<(), vmm_devices::FlushError> {
        Lifecycle::flush_backing(&self.device, intent)
    }

    fn post_restore_kick(&self) {
        // Re-inject interrupts on all enabled queues after vCPUs are
        // running. This ensures the guest's NAPI handler runs and
        // replenishes rx buffers.
        let vs = self.virtio_state.lock().expect("virtio lock");
        let enabled: Vec<u16> = (0..self.num_queues)
            .filter(|&idx| {
                vs.queue_enabled.get(idx as usize).copied().unwrap_or(false)
            })
            .collect();
        drop(vs);
        for idx in enabled {
            self.raise_queue_interrupt_current(idx);
        }
    }

    fn migrate(&'_ self) -> Migrator<'_> {
        Lifecycle::migrate(&self.device)
    }

    fn export_virtio_state(
        &self,
    ) -> Vec<vmm_devices::lifecycle::VirtioMigrateQueue> {
        self.export_migrate_queues()
    }

    fn restore_virtio_state(
        &self,
        states: &[vmm_devices::lifecycle::VirtioMigrateQueue],
    ) {
        self.restore_migrate_queues(states);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod reset_tests;
