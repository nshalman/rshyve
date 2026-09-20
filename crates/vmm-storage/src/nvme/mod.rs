// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! NVMe 1.0 controller with one namespace.
//!
//! Admin commands run on the vCPU that rang the doorbell. I/O commands
//! go through a channel to a pool of worker threads that move data
//! straight between the backing file and the guest pages named by the
//! PRPs, then post the completion and raise MSI-X.
//!
//! BAR0 holds the registers and doorbells, BAR4 the MSI-X table.

pub mod bits;

mod admin;
mod io;
mod prp;
mod queues;
mod regs;
mod workers;

#[cfg(test)]
mod tests;

use std::fs::File;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use vmm_core::common::RWOp;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::PhysMap;
use vmm_core::mmio::MmioBus;

use vmm_devices::blkdev::DiskCache;
use vmm_devices::lifecycle::Indicator;
use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{
    replay_migrate_pci_state, DeviceIdent, DeviceState, PciDevice,
};
use vmm_devices::pci::msix::{HdlMsiSink, MsiSink, MsixTable};
use vmm_devices::pci::BarN;
use vmm_devices::{
    DeviceMigrateState, FlushError, FlushIntent, Lifecycle, QuiesceGate,
};

use bits::*;
use io::{IoBackend, IoRequest};
use queues::{CompQueue, NvmeState, SubQueue};
use workers::io_worker;

/// Matches the C bhyve blockif thread count.
const NUM_WORKERS: usize = 8;

/// How long a reset waits for the transfers already moving guest
/// memory. Past this the controller reports a fatal status: the driver
/// is about to reuse pages a worker still holds.
const WORKER_FENCE_BUDGET: Duration = Duration::from_secs(5);

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
/// demand; production passes the VM handle and the file itself.
pub(super) struct NvmeParts {
    pub(super) file: Arc<File>,
    pub(super) backend: Arc<dyn IoBackend>,
    pub(super) read_only: bool,
    pub(super) instance_id: u8,
    pub(super) physmap: Arc<PhysMap>,
    pub(super) msi: Arc<dyn MsiSink>,
    pub(super) bus_mmio: Arc<MmioBus>,
    pub(super) log: slog::Logger,
}

/// NVMe 1.0 controller implementing the PCI device trait.
pub struct NvmeController {
    pub(super) pci_state: Mutex<DeviceState>,
    pub(super) msix: Arc<MsixTable>,
    state: Mutex<NvmeState>,
    io_tx: Mutex<Option<mpsc::Sender<IoRequest>>>,
    pub(super) physmap: Arc<PhysMap>,
    pub(super) bus_mmio: Arc<MmioBus>,
    pub(super) registered_bar0: Mutex<Option<u64>>,
    pub(super) registered_bar4: Mutex<Option<u64>>,
    /// Weak self-reference for MMIO handler closures.
    pub(super) self_ref: Mutex<Option<std::sync::Weak<Self>>>,
    pub(super) log: slog::Logger,
    /// Zvol write-cache control.
    _cache: DiskCache,
    /// The backing store, kept for the durable flush.
    file: Arc<File>,
    /// The I/O worker pool, joined by [`Self::stop_workers`].
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
    pub(super) geometry: Geometry,
    indicator: Indicator,
    pub(super) gate: Arc<QuiesceGate>,
}

impl NvmeController {
    /// Create a new NVMe controller.
    ///
    /// Bus registration failures go to a discarding logger. Use
    /// [`new_with_logger`](Self::new_with_logger) to see them.
    pub fn new(
        file: File,
        read_only: bool,
        instance_id: u8,
        physmap: Arc<PhysMap>,
        hdl: Arc<VmmHdl>,
        bus_mmio: Arc<MmioBus>,
    ) -> Arc<Self> {
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
    /// diagnostics.
    pub fn new_with_logger(
        file: File,
        read_only: bool,
        instance_id: u8,
        physmap: Arc<PhysMap>,
        hdl: Arc<VmmHdl>,
        bus_mmio: Arc<MmioBus>,
        log: slog::Logger,
    ) -> Arc<Self> {
        let file = Arc::new(file);
        Self::build(NvmeParts {
            backend: Arc::clone(&file) as Arc<dyn IoBackend>,
            file,
            read_only,
            instance_id,
            physmap,
            msi: HdlMsiSink::new(hdl),
            bus_mmio,
            log,
        })
    }

    pub(super) fn build(parts: NvmeParts) -> Arc<Self> {
        let NvmeParts {
            file,
            backend,
            read_only,
            instance_id,
            physmap,
            msi,
            bus_mmio,
            log,
        } = parts;

        // NVMe has explicit FLUSH semantics, so the zvol may run
        // writeback. Set before any I/O so first writes hit the fast path.
        let cache = DiskCache::new(Arc::clone(&file), read_only);

        let file_len = file
            .metadata()
            .expect("nvme: failed to stat backing file")
            .len();
        let block_size: u32 = 512;
        let geometry = Geometry {
            block_size,
            total_blocks: file_len / u64::from(block_size),
        };

        let serial = format!("TRITON-NVME-{:04}", instance_id);
        let ctrl_ident = build_identify_controller(
            &serial,
            "Triton VMM NVMe Controller",
            "1.0.0",
            1,
            MDTS_LOG2_PAGES,
            // One controller per instance, and CNTLID 0 is reserved.
            u16::from(instance_id) + 1,
        );
        let ns_ident =
            build_identify_namespace(geometry.total_blocks, block_size);

        // CQR: contiguous queues only. TO: 500 ms. DSTRD, MPSMIN and
        // MPSMAX are zero: 4-byte doorbells, 4 KiB pages.
        let cap: u64 = (u64::from(MAX_QUEUE_SIZE - 1) << CAP_MQES_SHIFT)
            | CAP_CQR
            | (1u64 << CAP_TO_SHIFT)
            | CAP_CSS_NVM;

        let ident = DeviceIdent {
            vendor_id: PCI_VENDOR_ID,
            device_id: PCI_DEVICE_ID,
            class: PCI_CLASS_STORAGE,
            subclass: PCI_SUBCLASS_NVM,
            prog_if: PCI_PROGIF_NVME,
            revision: 1,
            sub_vendor_id: PCI_VENDOR_ID,
            sub_device_id: PCI_DEVICE_ID,
        };

        let mut pci_state = DeviceState::new(ident);
        // A 32-bit BAR0 keeps the firmware placing it in the 3-4 GiB
        // MMIO gap, where EPT traps reliably.
        pci_state.define_bar(BarN::BAR0, BarDefine::Mmio(BAR0_SIZE as u32));

        let msix = Arc::new(MsixTable::new(NVME_MSIX_COUNT, msi));
        let msix_bar_size = msix.bar_size() as u32;
        pci_state.define_bar(
            BarN::BAR4,
            BarDefine::Mmio(msix_bar_size.next_power_of_two().max(4096)),
        );
        pci_state.set_cap_ptr(MSIX_CAP_OFFSET);

        let nvme_state = NvmeState {
            cap,
            cc: 0,
            csts: 0,
            aqa: 0,
            asq_base: 0,
            acq_base: 0,
            admin_sq: None,
            admin_cq: None,
            io_sqs: Default::default(),
            io_cqs: Default::default(),
            ctrl_ident,
            ns_ident,
            num_io_queues: MAX_IO_QUEUES as u16,
            instances: 0,
        };

        let (tx, rx) = mpsc::channel::<IoRequest>();
        let gate = Arc::new(QuiesceGate::new(NUM_WORKERS));

        let ctrl = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            msix: Arc::clone(&msix),
            state: Mutex::new(nvme_state),
            io_tx: Mutex::new(Some(tx)),
            physmap: Arc::clone(&physmap),
            bus_mmio,
            registered_bar0: Mutex::new(None),
            registered_bar4: Mutex::new(None),
            self_ref: Mutex::new(None),
            log,
            _cache: cache,
            file,
            workers: Mutex::new(Vec::with_capacity(NUM_WORKERS)),
            geometry,
            indicator: Indicator::new(),
            gate: Arc::clone(&gate),
        });
        *ctrl.self_ref.lock().expect("self_ref lock") =
            Some(Arc::downgrade(&ctrl));

        let rx = Arc::new(Mutex::new(rx));
        let mut workers = Vec::with_capacity(NUM_WORKERS);
        for i in 0..NUM_WORKERS {
            let rx = Arc::clone(&rx);
            let weak = Arc::downgrade(&ctrl);
            let gate = Arc::clone(&gate);
            let backend = Arc::clone(&backend);

            workers.push(
                thread::Builder::new()
                    .name(format!("nvme-io-{}", i))
                    .spawn(move || {
                        io_worker(rx, backend, read_only, weak, gate);
                    })
                    .expect("nvme: failed to spawn I/O worker thread"),
            );
        }
        *ctrl.workers.lock().expect("nvme: workers lock") = workers;

        ctrl
    }

    /// Wait for the transfers that are already moving guest memory.
    ///
    /// Call with the state lock dropped: a worker needs it to post its
    /// completion before it can park. Returns whether the workers
    /// stopped inside the budget.
    pub(super) fn fence_workers(&self) -> bool {
        let _drain = self.gate.drain_scope();
        if self.gate.wait_quiesced(WORKER_FENCE_BUDGET) {
            return true;
        }
        slog::error!(self.log, "nvme: I/O workers did not stop for a reset");
        false
    }

    /// Disconnect the I/O channel and wait for the workers.
    ///
    /// Dropping the sender is the only thing that ends a worker, and
    /// nothing else releases the backing fd or lets `DiskCache` put the
    /// zvol write cache back the way it found it.
    fn stop_workers(&self) {
        drop(self.io_tx.lock().expect("nvme: io_tx lock").take());
        // A parked worker never reaches the channel to see it close.
        self.gate.resume();
        let workers =
            std::mem::take(&mut *self.workers.lock().expect("workers lock"));
        for worker in workers {
            if worker.join().is_err() {
                slog::error!(self.log, "nvme: I/O worker panicked");
            }
        }
    }
}

impl Drop for NvmeController {
    fn drop(&mut self) {
        self.stop_workers();
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

    fn pause(&self) {
        self.indicator.pause();
        self.gate.pause();
    }

    fn is_quiesced(&self) -> bool {
        self.gate.is_quiesced()
    }

    fn resume(&self) {
        self.gate.resume();
        self.indicator.resume();
    }

    fn halt(&self) {
        self.indicator.halt();
        self.stop_workers();
    }

    fn flush_backing(&self, intent: FlushIntent) -> Result<(), FlushError> {
        vmm_devices::quiesce::flush_file(&self.gate, &self.file, intent, "nvme")
    }

    fn export_migrate_state(&self) -> Option<DeviceMigrateState> {
        use vmm_devices::lifecycle::{
            NvmeMigrateCq, NvmeMigrateSq, NvmeMigrateState,
        };

        let st = self.state.lock().expect("nvme state lock");

        // Only export if the controller is enabled (has active queues)
        if (st.cc & CC_EN) == 0 {
            return None;
        }

        let admin_sq = st.admin_sq.as_ref().map(|sq| NvmeMigrateSq {
            base: sq.base,
            size: sq.size,
            head: sq.head,
            tail: sq.tail,
            cq_id: sq.cq_id,
        });
        let admin_cq = st.admin_cq.as_ref().map(|cq| NvmeMigrateCq {
            base: cq.base,
            size: cq.size,
            head: cq.head,
            tail: cq.tail,
            phase: cq.phase,
            iv: cq.iv,
        });
        let io_sqs = st
            .io_sqs
            .iter()
            .map(|sq| {
                sq.as_ref().map(|sq| NvmeMigrateSq {
                    base: sq.base,
                    size: sq.size,
                    head: sq.head,
                    tail: sq.tail,
                    cq_id: sq.cq_id,
                })
            })
            .collect();
        let io_cqs = st
            .io_cqs
            .iter()
            .map(|cq| {
                cq.as_ref().map(|cq| NvmeMigrateCq {
                    base: cq.base,
                    size: cq.size,
                    head: cq.head,
                    tail: cq.tail,
                    phase: cq.phase,
                    iv: cq.iv,
                })
            })
            .collect();

        let pci = self.pci_state.lock().expect("pci lock").migrate_state();
        let pci_command = pci.command;
        let pci_bar_addrs = pci.bar_addrs;

        // Export MSI-X table entries
        let msix_count = self.msix.count();
        let msix_entries: Vec<(u64, u64)> =
            (0..msix_count).map(|v| self.msix.read_entry(v)).collect();
        let msix_enabled = self.msix.is_enabled();

        slog::info!(self.log, "nvme: exporting controller state";
            "cc" => format!("{:#x}", st.cc),
            "csts" => format!("{:#x}", st.csts),
            "aqa" => format!("{:#x}", st.aqa),
            "io_queues" => st.num_io_queues,
            "msix_enabled" => msix_enabled);

        Some(DeviceMigrateState::Nvme(NvmeMigrateState {
            cc: st.cc,
            csts: st.csts,
            aqa: st.aqa,
            asq_base: st.asq_base,
            acq_base: st.acq_base,
            admin_sq,
            admin_cq,
            io_sqs,
            io_cqs,
            num_io_queues: st.num_io_queues,
            pci_command,
            pci_bar_addrs,
            msix_entries,
            msix_enabled,
        }))
    }

    fn restore_migrate_state(&self, state: &DeviceMigrateState) {
        let DeviceMigrateState::Nvme(mig) = state else {
            return;
        };
        slog::info!(self.log, "nvme: restoring controller state";
            "cc" => format!("{:#x}", mig.cc),
            "csts" => format!("{:#x}", mig.csts),
            "aqa" => format!("{:#x}", mig.aqa),
            "io_queues" => mig.num_io_queues,
            "msix_enabled" => mig.msix_enabled);

        // Restore PCI BAR addresses and command register
        let bar_writes = self
            .pci_state
            .lock()
            .expect("pci lock")
            .bar_replay_writes(&mig.pci_bar_addrs);
        replay_migrate_pci_state(self, &bar_writes, mig.pci_command);

        // Restore controller state
        let mut st = self.state.lock().expect("nvme state lock");
        st.cc = mig.cc;
        st.csts = mig.csts;
        st.aqa = mig.aqa;
        st.asq_base = mig.asq_base;
        st.acq_base = mig.acq_base;
        st.num_io_queues = mig.num_io_queues.clamp(1, MAX_IO_QUEUES as u16);

        // One fresh instance for every imported queue: nothing from
        // before the import may complete into one.
        let instance = st.next_instance();

        // Restore admin queues
        st.admin_sq = mig.admin_sq.as_ref().map(|sq| SubQueue {
            base: sq.base,
            size: sq.size,
            head: sq.head,
            tail: sq.tail,
            cq_id: sq.cq_id,
            instance,
        });
        st.admin_cq = mig.admin_cq.as_ref().map(|cq| CompQueue {
            base: cq.base,
            size: cq.size,
            head: cq.head,
            tail: cq.tail,
            phase: cq.phase,
            iv: cq.iv,
            ien: true,
            instance,
        });

        // Restore I/O queues
        for (idx, sq_opt) in mig.io_sqs.iter().enumerate() {
            if idx < MAX_IO_QUEUES {
                st.io_sqs[idx] = sq_opt.as_ref().map(|sq| SubQueue {
                    base: sq.base,
                    size: sq.size,
                    head: sq.head,
                    tail: sq.tail,
                    cq_id: sq.cq_id,
                    instance,
                });
            }
        }
        for (idx, cq_opt) in mig.io_cqs.iter().enumerate() {
            if idx < MAX_IO_QUEUES {
                st.io_cqs[idx] = cq_opt.as_ref().map(|cq| CompQueue {
                    base: cq.base,
                    size: cq.size,
                    head: cq.head,
                    tail: cq.tail,
                    phase: cq.phase,
                    iv: cq.iv,
                    ien: true,
                    instance,
                });
            }
        }
        drop(st);

        // Restore MSI-X table entries and enable flag
        for (v, &(addr, data)) in mig.msix_entries.iter().enumerate() {
            self.msix.write_entry(v as u16, addr, data);
        }
        self.msix.set_enabled(mig.msix_enabled);
    }
}
