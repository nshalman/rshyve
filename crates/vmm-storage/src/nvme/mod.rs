// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! NVMe 1.0 controller with one namespace.
//!
//! Admin commands run on the vCPU that rang the doorbell. I/O commands
//! go to an [`AsyncBackend`](backend::AsyncBackend), which completes
//! them later from a thread of its own. The default backend is a worker
//! pool over the backing file.
//!
//! BAR0 holds the registers and doorbells, BAR4 the MSI-X table.

pub mod backend;
pub mod bits;

mod admin;
mod complete;
mod config;
mod cq;
mod inflight;
mod io;
mod locked;
mod migrate;
mod pool;
mod prp;
mod queues;
mod regs;

#[cfg(test)]
mod tests;

use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use vmm_core::common::RWOp;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::PhysMap;
use vmm_core::mmio::MmioBus;

use vmm_devices::blkdev::DiskSync;
use vmm_devices::lifecycle::{DeviceStateError, Indicator, NvmeMigrateBackend};
use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{DeviceIdent, DeviceState, PciDevice};
use vmm_devices::pci::msix::{HdlMsiSink, MsiSink, MsixTable};
use vmm_devices::pci::BarN;
use vmm_devices::{DeviceMigrateState, FlushError, FlushIntent, Lifecycle};

pub use bits::NsHints;
pub use config::{LbaSize, MsiPolicy, NvmeConfig};

use backend::{AsyncBackend, BackendFactory};
use bits::*;
use complete::Completions;
use io::IoBackend;
use pool::WorkerPool;
use queues::NvmeState;

#[cfg(test)]
use pool::NUM_WORKERS;

/// How long a controller transition waits for the commands in flight.
#[derive(Clone, Copy)]
pub(super) struct FenceBudgets {
    /// A controller reset or a Delete Queue command.
    pub(super) reset: Duration,
    /// A shutdown, which flushes the write cache as well. This is the
    /// budget the durable flush used to spend on its own quiesce.
    pub(super) shutdown: Duration,
}

const FENCE_BUDGETS: FenceBudgets = FenceBudgets {
    reset: Duration::from_secs(5),
    shutdown: Duration::from_secs(30),
};

/// MSI-X capability offset in PCI config space.
const MSIX_CAP_OFFSET: u8 = 0x40;

/// Namespace geometry, fixed for the life of the controller.
#[derive(Clone, Copy)]
pub(super) struct Geometry {
    pub(super) block_size: u32,
    pub(super) total_blocks: u64,
}

/// What a controller is built from.
///
/// Tests inject a recording MSI sink and a backend that stalls on
/// demand; production passes the VM handle and the disk itself.
pub(super) struct NvmeParts {
    pub(super) backend: BackendFactory,
    pub(super) read_only: bool,
    pub(super) instance_id: u8,
    pub(super) physmap: Arc<PhysMap>,
    pub(super) msi: Arc<dyn MsiSink>,
    pub(super) bus_mmio: Arc<MmioBus>,
    pub(super) fences: FenceBudgets,
    pub(super) config: NvmeConfig,
    pub(super) log: slog::Logger,
}

/// Read without the state lock, but changed only under it.
#[derive(Default)]
struct Counts {
    /// Commands admitted and not yet finished, every queue.
    in_flight: AtomicUsize,
    /// Threads between releasing the state lock and raising the MSIs
    /// it owed.
    raising: AtomicUsize,
    paused: AtomicBool,
    /// Lets a test tell a drain from the idle short cut.
    drains: AtomicUsize,
}

impl Counts {
    fn admitted(&self) {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
    }

    fn quiesced(&self) -> bool {
        self.paused.load(Ordering::Acquire)
            && self.in_flight.load(Ordering::Acquire) == 0
            && self.raising.load(Ordering::Acquire) == 0
    }
}

/// NVMe 1.0 controller implementing the PCI device trait.
pub struct NvmeController {
    pub(super) pci_state: Mutex<DeviceState>,
    pub(super) msix: Arc<MsixTable>,
    state: Mutex<NvmeState>,
    /// Woken when a command leaves flight while a drain waits.
    drained: Condvar,
    /// Completions a backend made inside `submit`, finished when it
    /// returns.
    deferred: Mutex<Vec<(backend::RequestId, backend::BlockStatus)>>,
    backend: Arc<dyn AsyncBackend>,
    pub(super) physmap: Arc<PhysMap>,
    pub(super) bus_mmio: Arc<MmioBus>,
    pub(super) registered_bar0: Mutex<Option<u64>>,
    pub(super) registered_bar4: Mutex<Option<u64>>,
    /// Weak self-reference for MMIO handler closures.
    pub(super) self_ref: Mutex<Option<std::sync::Weak<Self>>>,
    pub(super) log: slog::Logger,
    pub(super) geometry: Geometry,
    read_only: bool,
    /// The most commands one I/O SQ may have admitted at once.
    depth: u16,
    indicator: Indicator,
    counts: Counts,
    pub(super) fences: FenceBudgets,
    config: NvmeConfig,
}

impl NvmeController {
    /// Create a new NVMe controller.
    ///
    /// Bus registration and write cache warnings go to a discarding
    /// logger. Use [`new_with_logger`](Self::new_with_logger) to see
    /// them.
    pub fn new(
        file: File,
        read_only: bool,
        instance_id: u8,
        physmap: Arc<PhysMap>,
        hdl: Arc<VmmHdl>,
        bus_mmio: Arc<MmioBus>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::new_with_logger(
            file,
            read_only,
            instance_id,
            physmap,
            hdl,
            bus_mmio,
            slog::Logger::root(slog::Discard, slog::o!()),
        )
    }

    /// Create a new NVMe controller with a logger for bus registration
    /// diagnostics and zvol write cache warnings.
    pub fn new_with_logger(
        file: File,
        read_only: bool,
        instance_id: u8,
        physmap: Arc<PhysMap>,
        hdl: Arc<VmmHdl>,
        bus_mmio: Arc<MmioBus>,
        log: slog::Logger,
    ) -> anyhow::Result<Arc<Self>> {
        // Turns a zvol's write cache on before any I/O, so first writes
        // already hit the fast path.
        let disk = Arc::new(DiskSync::new(Arc::new(file), read_only, &log)?);
        let backend = WorkerPool::factory(
            Arc::clone(&disk),
            disk as Arc<dyn IoBackend>,
            log.clone(),
        )?;
        Self::with_backend(
            backend,
            NvmeConfig::default(),
            read_only,
            instance_id,
            physmap,
            hdl,
            bus_mmio,
            log,
        )
    }

    /// Fails when `config` reports no volatile write cache and the
    /// backend's writes are not durable when they complete.
    pub fn with_backend(
        backend: BackendFactory,
        config: NvmeConfig,
        read_only: bool,
        instance_id: u8,
        physmap: Arc<PhysMap>,
        hdl: Arc<VmmHdl>,
        bus_mmio: Arc<MmioBus>,
        log: slog::Logger,
    ) -> anyhow::Result<Arc<Self>> {
        Self::build(NvmeParts {
            backend,
            read_only,
            instance_id,
            physmap,
            msi: HdlMsiSink::new(hdl),
            bus_mmio,
            fences: FENCE_BUDGETS,
            config,
            log,
        })
    }

    pub(super) fn build(parts: NvmeParts) -> anyhow::Result<Arc<Self>> {
        let NvmeParts {
            backend,
            read_only,
            instance_id,
            physmap,
            msi,
            bus_mmio,
            fences,
            config,
            log,
        } = parts;

        let msix = Arc::new(MsixTable::new(NVME_MSIX_COUNT, msi));
        let pci_state = pci_header(&msix);

        let ctrl = Arc::new_cyclic(|weak| {
            let backend = backend(Arc::new(Completions(weak.clone())));
            let block_size = config.lba_size.bytes();
            let geometry = Geometry {
                block_size,
                total_blocks: backend.capacity() / u64::from(block_size),
            };
            // A depth of 0 would admit nothing and stall every queue.
            // Past the largest queue a migration payload may hold
            // completions for, a state this controller made would not
            // restore.
            let depth = backend.queue_depth().clamp(1, MAX_QUEUE_SIZE);
            let state = initial_state(
                backend.as_ref(),
                geometry,
                config,
                instance_id,
                read_only,
            );

            Self {
                pci_state: Mutex::new(pci_state),
                msix,
                state: Mutex::new(state),
                drained: Condvar::new(),
                deferred: Mutex::new(Vec::new()),
                backend,
                physmap,
                bus_mmio,
                registered_bar0: Mutex::new(None),
                registered_bar4: Mutex::new(None),
                self_ref: Mutex::new(Some(weak.clone())),
                log,
                geometry,
                read_only,
                depth,
                indicator: Indicator::new(),
                counts: Counts::default(),
                fences,
                config,
            }
        });
        // Dropping the controller here halts the backend it built.
        if !config.volatile_write_cache && !ctrl.backend.durable_writes() {
            anyhow::bail!(
                "nvme: no volatile write cache needs a backend whose \
                 writes are durable when they complete"
            );
        }
        Ok(ctrl)
    }

    #[cfg(test)]
    pub(super) fn drain_count(&self) -> usize {
        self.counts.drains.load(Ordering::Relaxed)
    }
}

fn pci_header(msix: &MsixTable) -> DeviceState {
    let mut pci_state = DeviceState::new(DeviceIdent {
        vendor_id: PCI_VENDOR_ID,
        device_id: PCI_DEVICE_ID,
        class: PCI_CLASS_STORAGE,
        subclass: PCI_SUBCLASS_NVM,
        prog_if: PCI_PROGIF_NVME,
        revision: 1,
        sub_vendor_id: PCI_VENDOR_ID,
        sub_device_id: PCI_DEVICE_ID,
    });
    // A 32-bit BAR0 keeps the firmware placing it in the 3-4 GiB MMIO
    // gap, where EPT traps reliably.
    pci_state.define_bar(BarN::BAR0, BarDefine::Mmio(BAR0_SIZE as u32));
    let msix_bar_size = msix.bar_size() as u32;
    pci_state.define_bar(
        BarN::BAR4,
        BarDefine::Mmio(msix_bar_size.next_power_of_two().max(4096)),
    );
    pci_state.set_cap_ptr(MSIX_CAP_OFFSET);
    pci_state
}

fn initial_state(
    backend: &dyn AsyncBackend,
    geometry: Geometry,
    config: NvmeConfig,
    instance_id: u8,
    read_only: bool,
) -> NvmeState {
    let serial = format!("TRITON-NVME-{:04}", instance_id);
    let ctrl_ident = build_identify_controller(
        &serial,
        "Triton VMM NVMe Controller",
        "1.0.0",
        1,
        MDTS_LOG2_PAGES,
        // One controller per instance, and CNTLID 0 is reserved.
        u16::from(instance_id) + 1,
        config.volatile_write_cache,
        if config.deallocate {
            ONCS_DSM | ONCS_WRITE_ZEROES
        } else {
            0
        },
    );
    let ns_ident = build_identify_namespace(
        geometry.total_blocks,
        geometry.block_size,
        read_only,
        config.hints,
        config.deallocate,
    );

    // CQR: contiguous queues only. TO: 500 ms. DSTRD, MPSMIN and MPSMAX
    // are zero: 4-byte doorbells, 4 KiB pages.
    let cap: u64 = (u64::from(MAX_QUEUE_SIZE - 1) << CAP_MQES_SHIFT)
        | CAP_CQR
        | (1u64 << CAP_TO_SHIFT)
        | CAP_CSS_NVM;

    let max_queues = backend.max_queues().clamp(1, MAX_IO_QUEUES as u16);
    NvmeState {
        cap,
        ctrl_ident,
        ns_ident,
        num_io_sqs: max_queues,
        num_io_cqs: max_queues,
        max_io_queues: max_queues,
        volatile_write_cache: config.volatile_write_cache,
        ..NvmeState::default()
    }
}

impl Drop for NvmeController {
    fn drop(&mut self) {
        // Nothing else releases the backing fd or lets `DiskSync` turn
        // off a zvol write cache it turned on.
        self.backend.halt();
    }
}

// ---------------------------------------------------------------------------
// PciDevice trait implementation
// ---------------------------------------------------------------------------

impl PciDevice for NvmeController {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        // Standard header: 0x00..0x3F
        if offset < 0x40 {
            let pci = self.pci_state.lock().expect("nvme: pci_state lock");
            return pci.cfg_read(offset, len);
        }

        // MSI-X capability: 0x40..0x4B
        // cap_read returns a full dword; extract the requested bytes.
        let dword_offset = offset & 0xFC;
        if let Some(dword_val) =
            self.msix.cap_read(dword_offset, MSIX_CAP_OFFSET)
        {
            let byte_off = (offset & 0x03) as u32;
            let mask = match len {
                1 => 0xFF,
                2 => 0xFFFF,
                _ => 0xFFFF_FFFF,
            };
            return (dword_val >> (byte_off * 8)) & mask;
        }

        0
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        if offset < 0x40 {
            let mut pci = self.pci_state.lock().expect("nvme: pci_state lock");
            pci.cfg_write(offset, len, val);

            // Check if BAR or command register changed
            let dword_off = offset & 0xFC;
            if dword_off == 0x04 || (0x10..=0x24).contains(&dword_off) {
                let weak = self.self_ref.lock().expect("self_ref lock");
                if let Some(arc_self) = weak.as_ref().and_then(|w| w.upgrade())
                {
                    drop(pci);
                    arc_self.update_mmio_registration();
                }
            }
            return;
        }

        // MSI-X capability — reconstruct the full dword value
        // by reading the current dword and merging the written bytes.
        let dword_offset = offset & 0xFC;
        let byte_off = (offset & 0x03) as u32;
        if let Some(cur_dword) =
            self.msix.cap_read(dword_offset, MSIX_CAP_OFFSET)
        {
            let mask = match len {
                1 => 0xFFu32,
                2 => 0xFFFFu32,
                _ => 0xFFFF_FFFFu32,
            };
            let shifted_mask = mask << (byte_off * 8);
            let shifted_val = (val & mask) << (byte_off * 8);
            let merged = (cur_dword & !shifted_mask) | shifted_val;
            self.msix.cap_write(dword_offset, MSIX_CAP_OFFSET, merged);
        }
    }

    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        match bar {
            BarN::BAR0 => self.bar0_rw(offset, rwo),
            BarN::BAR4 => self.bar4_rw(offset, rwo),
            _ => {
                // Unexpected BAR access; read returns 0, write ignored
                if let RWOp::Read(ro) = rwo {
                    ro.write_u32(0);
                }
            }
        }
    }

    fn detach_regions(&self) {
        for (bar, reg_mutex) in [
            (BarN::BAR0, &self.registered_bar0),
            (BarN::BAR4, &self.registered_bar4),
        ] {
            let mut reg = reg_mutex.lock().expect("bar reg lock");
            self.release_mmio(bar, &mut reg);
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle implementation
// ---------------------------------------------------------------------------

impl Lifecycle for NvmeController {
    fn type_name(&self) -> &'static str {
        "nvme"
    }

    fn lifecycle_state(
        &self,
    ) -> Option<vmm_devices::lifecycle::IndicatedState> {
        Some(self.indicator.state())
    }

    fn start(&self) -> anyhow::Result<()> {
        self.indicator.start();
        Ok(())
    }

    /// Close admission: the doorbell handler records the tail and
    /// takes nothing until [`Self::resume`].
    fn pause(&self) {
        self.indicator.pause();
        let mut st = self.lock();
        st.admission.paused = true;
        self.counts.paused.store(true, Ordering::Release);
    }

    fn is_quiesced(&self) -> bool {
        self.counts.quiesced()
    }

    /// Open admission, and take what the guest queued while paused:
    /// its doorbells are spent, and only a doorbell starts the submit
    /// path otherwise.
    fn resume(&self) {
        let mut st = self.lock();
        st.admission.paused = false;
        self.counts.paused.store(false, Ordering::Release);
        // A payload may hold completions for a CQ with room, and only a
        // CQ head doorbell writes them otherwise. A guest with nothing
        // left to consume rings none.
        self.flush_all_held(&mut st);
        self.admit_all(&mut st);
        drop(st);
        self.indicator.resume();
    }

    fn halt(&self) {
        self.indicator.halt();
        self.lock().admission.halted = true;
        self.backend.halt();
    }

    /// Admission stays closed across the drain and the flush, so the
    /// flush covers every write the guest has seen complete.
    fn flush_backing(&self, intent: FlushIntent) -> Result<(), FlushError> {
        let budget = vmm_devices::quiesce::quiesce_budget(intent);
        let (st, drained) = self.drain_all(self.lock(), budget);
        // The sync can take seconds; completions need the lock.
        drop(st);
        let result = if drained {
            self.backend.flush(intent)
        } else {
            Err(FlushError::NotQuiesced("nvme"))
        };
        self.reopen(&mut self.lock());
        result
    }

    fn migration_blocker(&self) -> Option<String> {
        if self.backend.migratable() {
            return None;
        }
        Some(match self.backend.identity() {
            NvmeMigrateBackend::MantaBlock { volume } => format!(
                "live migration of a MantaBlock disk is not supported yet \
                 (volume {volume})"
            ),
            other => format!("nvme on {other} has no migration handover yet"),
        })
    }

    fn export_migrate_state(
        &self,
    ) -> Result<Option<DeviceMigrateState>, DeviceStateError> {
        self.export_state()
    }

    fn restore_migrate_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        self.restore_state(state)
    }
}
