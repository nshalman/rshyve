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
//!
//! # Where the parts are
//!
//! This module keeps what every path shares: the register state in
//! `VirtioPciState`, the reset latch inside it, and the reset
//! protocol that closes the latch and reopens it. Each submodule owns
//! one thing, and its own doc says which:
//!
//! - `bus`: the addresses this device decodes, and the accesses that
//!   arrive at them.
//! - `legacy`: the pre-1.0 register file on BAR0.
//! - `modern`: the capability chain and the BAR2 register file it
//!   points at.
//! - `intr`: every interrupt this transport delivers, ordered against
//!   its own reset.
//! - `migrate`: what goes on the migration wire, and the order a
//!   restore puts it back in.

use std::sync::{Arc, Mutex};

use vmm_core::mem::PhysMap;
use vmm_core::mmio::MmioBus;
use vmm_core::pio::PioBus;

use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{DeviceIdent, DeviceState};
use vmm_devices::pci::msix::MsixTable;
use vmm_devices::pci::{BarN, IntrPin, LintrCfg};
use vmm_devices::DeviceMigrateState;

use super::bits;
use super::queue::VirtQueue;
use super::VirtioDevice;

mod bus;
pub mod intr;
mod legacy;
mod migrate;
mod modern;
pub mod notify;

use intr::IntrGate;
use modern::CapOffsets;

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
    /// Whether the driver has made each queue live.
    ///
    /// Modern drivers write QUEUE_ENABLE; legacy drivers have no such
    /// register, so a non-zero QUEUE_PFN sets this instead. A kick is
    /// gated on the ring addresses rather than on this, which is what a
    /// migration export reads to decide which queues to carry.
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

    /// Tell the queues what the driver negotiated.
    ///
    /// The ring features decide what the device reads and writes, so
    /// they are applied once, where negotiation ends, and never under
    /// a live ring: a `VirtioCompletion` takes EVENT_IDX when it is
    /// built and would then judge kicks by a rule its own queue no
    /// longer uses.
    fn apply_features(&mut self) {
        let event_idx =
            self.guest_features & bits::VIRTIO_F_RING_EVENT_IDX != 0;
        let indirect =
            self.guest_features & bits::VIRTIO_F_RING_INDIRECT_DESC != 0;
        for q in &mut self.queues {
            q.set_event_idx(event_idx);
            q.set_indirect_supported(indirect);
        }
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
    /// Reached by the post-restore kick once it has dropped the
    /// transport lock, which is the window a whole reset can run in.
    post_restore_kick_pending: ParkSlot,
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
/// Implements the [`PciDevice`](vmm_devices::pci::device::PciDevice)
/// trait so it can be attached to the PCI
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
    /// The notify addresses last given to the backend.
    notify_sent: Mutex<(u16, u64)>,
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
    /// The backend behind this transport.
    pub fn device(&self) -> &D {
        &self.device
    }

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
            notify_sent: Mutex::new((0, 0)),
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

    /// Whether a kick may reach the backend.
    ///
    /// A queue with no ring has nothing to drain, and a backend that
    /// builds its used-ring writer on the first kick would build it on
    /// address zero and keep it for the whole session, so the ring the
    /// driver programs next is never written.
    fn can_notify(vs: &VirtioPciState, queue_idx: u16) -> bool {
        vs.status & bits::STATUS_DRIVER_OK != 0
            && vs
                .queues
                .get(usize::from(queue_idx))
                .is_some_and(VirtQueue::is_configured)
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
        vs: std::sync::MutexGuard<'_, VirtioPciState>,
        idx: usize,
        error: Option<crate::queue::QueueAddrError>,
    ) {
        if !self.set_needs_reset(vs) {
            return;
        }
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
    }

    /// Apply a non-zero DEVICE_STATUS write, on either transport.
    ///
    /// VirtIO 1.3 sec 2.1.2: the driver must not clear a status bit.
    /// A device that let it would let a driver toggle FEATURES_OK and
    /// renegotiate under rings it has already programmed, or clear
    /// DEVICE_NEEDS_RESET and carry on with a device that stopped.
    /// The one way back is a write of 0, which the caller takes.
    ///
    /// The FEATURES_OK transition is where negotiation ends (sec
    /// 2.2.1), so it is where the backend and the rings are told what
    /// was agreed. A legacy driver never writes the bit; `legacy`
    /// applies its features on the feature register instead.
    fn set_status(
        &self,
        mut vs: std::sync::MutexGuard<'_, VirtioPciState>,
        new_status: u8,
    ) {
        if new_status & vs.status != vs.status {
            self.set_needs_reset(vs);
            return;
        }
        let gained = new_status & !vs.status;
        vs.status = new_status;
        if gained & bits::STATUS_FEATURES_OK != 0 {
            vs.apply_features();
            let features = vs.guest_features;
            self.device.set_features(features);
        }
    }

    /// Set DEVICE_NEEDS_RESET and tell the driver, once.
    ///
    /// Returns false when the device already carries the bit: the
    /// write that lands here is guest driven and repeatable, so the
    /// interrupt must not be raised per write. The bit goes in under
    /// the guard, which is then consumed: applied after the guard
    /// drops, it could land in the middle of a reset drain, or on the
    /// session that follows the reset.
    fn set_needs_reset(
        &self,
        mut vs: std::sync::MutexGuard<'_, VirtioPciState>,
    ) -> bool {
        if vs.status & bits::STATUS_DEVICE_NEEDS_RESET != 0 {
            return false;
        }
        vs.status |= bits::STATUS_DEVICE_NEEDS_RESET;
        // Sampled under the guard that wrote the bit, so a reset that
        // wipes it is always the later of the two.
        let session = self.intr.session();
        drop(vs);
        self.raise_config_interrupt_in(session);
        true
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

    /// Install the one-shot park the post-restore kick reaches once it
    /// has dropped the transport lock.
    #[cfg(test)]
    fn park_before_restore_kick(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .parks
            .post_restore_kick_pending
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
        // joins them: admission shut before the backend reset, so the
        // set is one injection per thread already inside and a guest
        // cannot add threads.
        //
        // WHY THE COUNT IS THE ONLY BOUND: the set is finite, its
        // members are not. Both injection ioctls take the VM lock as
        // readers, and an illumos reader yields to a waiting writer.
        // Memory hot-add holds that writer for hundreds of ms per GiB,
        // so a reset that lands beside one waits for it. Measured at
        // 1.3 us idle and 17.9 ms beside a 256 MiB hot-add. A deadline
        // here would not help: admission is tested before the ioctl,
        // so an injection already dispatched cannot be recalled, and
        // returning early would put it in the next driver session.
        // docs/hardware-run.md carries the measurement.
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

    /// Tell the driver the device-specific configuration changed.
    ///
    /// The generation moves on every change (VirtIO 1.3 sec 4.1.4.3), so
    /// a driver reading the config across it retries. The configuration
    /// change interrupt goes only to a driver at DRIVER_OK: one still
    /// initializing reads the config anyway, and has no handler yet. A
    /// reset in flight takes the config with it, so nothing is raised.
    pub fn signal_config_change(&self) {
        let Some(mut vs) = self.lock_for_write() else {
            return;
        };
        vs.config_generation = vs.config_generation.wrapping_add(1);
        if vs.status & bits::STATUS_DRIVER_OK == 0 {
            return;
        }
        // Sampled under the guard, as set_needs_reset does, so a reset
        // that follows is always the later of the two.
        let session = self.intr.session();
        drop(vs);
        self.raise_config_interrupt_in(session);
    }

    /// Signal the guest that the device needs a reset.
    ///
    /// A reset in flight takes the device down anyway, so the bit is
    /// dropped rather than written onto the session that follows.
    pub fn signal_device_needs_reset(&self) {
        let Some(vs) = self.lock_for_write() else {
            return;
        };
        self.set_needs_reset(vs);
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

    fn pause_within(&self, budget: std::time::Duration) {
        Lifecycle::pause_within(&self.device, budget)
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

    fn pause_for_migration(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        self.device.pause_rings_for_export()
    }

    fn flush_backing(
        &self,
        intent: vmm_devices::FlushIntent,
    ) -> Result<(), vmm_devices::FlushError> {
        Lifecycle::flush_backing(&self.device, intent)
    }

    fn backing_attached(&self) -> bool {
        Lifecycle::backing_attached(&self.device)
    }

    fn attach_backing(
        &self,
        how: &vmm_devices::lifecycle::BackingAttach,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        Lifecycle::attach_backing(&self.device, how)
    }

    fn detach_backing(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        Lifecycle::detach_backing(&self.device)
    }

    fn post_restore_kick(&self) {
        // Re-inject interrupts on all enabled queues after vCPUs are
        // running. This ensures the guest's NAPI handler runs and
        // replenishes rx buffers.
        //
        // WHY the session comes from under the lock: this runs on its
        // own thread for about a second after the vCPUs start, so the
        // guest can reset the device between the queue set being read
        // and the raise. A reset takes this same lock to end the
        // session, so a session sampled here names the driver whose
        // queues were just read, and one that no longer exists is
        // refused rather than written onto the driver that followed.
        let vs = self.virtio_state.lock().expect("virtio lock");
        let session = self.intr.session();
        let enabled: Vec<u16> = (0..self.num_queues)
            .filter(|&idx| {
                vs.queue_enabled.get(idx as usize).copied().unwrap_or(false)
            })
            .collect();
        drop(vs);
        #[cfg(test)]
        run_park(&self.parks.post_restore_kick_pending);
        for idx in enabled {
            self.raise_queue_interrupt_in(session, idx);
        }
    }

    fn migrate(&'_ self) -> Migrator<'_> {
        Lifecycle::migrate(&self.device)
    }

    fn export_migrate_state(
        &self,
    ) -> Result<
        Option<DeviceMigrateState>,
        vmm_devices::lifecycle::DeviceStateError,
    > {
        Ok(Some(DeviceMigrateState::Virtio(
            self.export_migrate_queues()?,
        )))
    }

    fn restore_migrate_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        match state {
            DeviceMigrateState::Virtio(state) => {
                self.restore_migrate_queues(state)
            }
            other => Err(vmm_devices::lifecycle::DeviceStateError::WrongKind {
                want: "virtio",
                got: other.kind(),
            }),
        }
    }

    fn resume_after_migration(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        self.device.resume_rings_after_migration()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod reset_tests;
