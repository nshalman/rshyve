// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! NVMe 1.0 controller device emulation.
//!
//! Implements a single-namespace NVMe controller with MSI-X interrupt
//! delivery and async I/O via worker threads. The vCPU thread submits
//! I/O requests through a channel and returns immediately; worker
//! threads perform file I/O, write completion queue entries to guest
//! memory, and fire MSI-X interrupts.
//!
//! # Architecture
//!
//! - BAR0: 64-bit MMIO (16 KiB) for controller registers + doorbells
//! - BAR4: MMIO for MSI-X table/PBA
//! - MSI-X capability at config offset 0x40
//! - Admin commands processed synchronously during doorbell write
//! - I/O commands dispatched to a pool of 4 worker threads
//! - File I/O uses `pread`/`pwrite` on the raw fd (lock-free)

pub mod bits;

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use vmm_core::common::RWOp;

/// Page size for NVMe PRP entries (4 KiB).
const PAGE_SIZE: usize = 4096;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::{MemCtx, PhysMap, SubMapping};
use vmm_core::mmio::{MmioBus, MmioFn};

use crate::blkdev::DiskCache;
use crate::lifecycle::Indicator;
use crate::pci::bar::BarDefine;
use crate::pci::device::{DeviceIdent, DeviceState, PciDevice};
use crate::pci::msix::MsixTable;
use crate::pci::BarN;
use crate::quiesce::datasync;
use crate::{FlushError, FlushIntent, Lifecycle, QuiesceGate};

use bits::*;

/// Number of I/O worker threads (matches C bhyve's blockif thread count).
const NUM_WORKERS: usize = 8;

/// MSI-X capability offset in PCI config space.
const MSIX_CAP_OFFSET: u8 = 0x40;

// ---------------------------------------------------------------------------
// Queue structures
// ---------------------------------------------------------------------------

struct SubQueue {
    /// Guest physical address of the queue base.
    base: u64,
    /// Number of entries (1-based, max entries).
    size: u16,
    /// Head pointer (consumer side, advanced by controller).
    head: u16,
    /// Tail pointer (updated by guest via doorbell write).
    tail: u16,
    /// Associated completion queue ID.
    cq_id: u16,
}

struct CompQueue {
    /// Guest physical address of the queue base.
    base: u64,
    /// Number of entries (1-based, max entries).
    size: u16,
    /// Head pointer (updated by guest via doorbell write).
    head: u16,
    /// Tail pointer (producer side, advanced by controller).
    tail: u16,
    /// Phase tag — toggles on each wrap of the CQ.
    phase: bool,
    /// MSI-X interrupt vector for this CQ.
    iv: u16,
}

// ---------------------------------------------------------------------------
// I/O request submitted to worker threads
// ---------------------------------------------------------------------------

struct IoRequest {
    opcode: u8,
    nsid: u32,
    slba: u64,
    nlb: u32,
    prp1: u64,
    prp2: u64,
    sqid: u16,
    cid: u16,
    cq_id: u16,
    /// Force Unit Access (NVMe 5.13, CDW12 bit 30). When set on a write
    /// we must fsync after pwritev so the data is durable before
    /// completion — the contract we're on the hook for because the
    /// Identify Controller page advertises VWC=1.
    fua: bool,
}

// ---------------------------------------------------------------------------
// NVMe controller state (under lock)
// ---------------------------------------------------------------------------

struct NvmeState {
    // Controller registers
    cap: u64,
    cc: u32,
    csts: u32,
    aqa: u32,
    asq_base: u64,
    acq_base: u64,

    // Queues
    admin_sq: Option<SubQueue>,
    admin_cq: Option<CompQueue>,
    io_sqs: [Option<SubQueue>; MAX_IO_QUEUES],
    io_cqs: [Option<CompQueue>; MAX_IO_QUEUES],

    // Pre-built Identify data
    ctrl_ident: [u8; IDENT_CTRL_SIZE],
    ns_ident: [u8; IDENT_NS_SIZE],

    // Configuration
    num_io_queues: u16,
    block_size: u32,
    total_blocks: u64,
}

// ---------------------------------------------------------------------------
// NVMe controller
// ---------------------------------------------------------------------------

/// NVMe 1.0 controller implementing the PCI device trait.
///
/// Provides a single namespace backed by a file, with MSI-X interrupts
/// and async I/O via worker threads.
pub struct NvmeController {
    pci_state: Mutex<DeviceState>,
    msix: Arc<MsixTable>,
    state: Mutex<NvmeState>,
    io_tx: Mutex<Option<mpsc::Sender<IoRequest>>>,
    physmap: Arc<PhysMap>,
    /// MMIO bus for BAR registration/unregistration.
    bus_mmio: Arc<MmioBus>,
    /// Currently registered BAR0 base address (if any).
    registered_bar0: Mutex<Option<u64>>,
    /// Currently registered BAR4 base address (if any).
    registered_bar4: Mutex<Option<u64>>,
    /// Weak self-reference for MMIO handler closures.
    self_ref: Mutex<Option<std::sync::Weak<Self>>>,
    /// Zvol write-cache control. Declared BEFORE `_file` so Drop runs
    /// `DKIOCSETWCE` restore while the fd is still open.
    _cache: DiskCache,
    /// Backing file kept alive for the raw fd used by I/O workers.
    /// Dropped (and fd closed) when the controller is dropped.
    _file: File,
    /// Lifecycle state indicator.
    indicator: Indicator,
    gate: Arc<QuiesceGate>,
}

impl NvmeController {
    /// Create a new NVMe controller.
    ///
    /// # Arguments
    ///
    /// - `file`: Backing storage file (owned, moved into workers).
    /// - `read_only`: If true, writes return an error status.
    /// - `physmap`: Guest physical memory map (shared with workers).
    /// - `hdl`: VMM handle for MSI-X interrupt delivery.
    pub fn new(
        file: File,
        read_only: bool,
        instance_id: u8,
        physmap: Arc<PhysMap>,
        hdl: Arc<VmmHdl>,
        bus_mmio: Arc<MmioBus>,
    ) -> Arc<Self> {
        let raw_fd = file.as_raw_fd();

        // Enable zvol writeback cache (DKIOCSETWCE=1). NVMe's protocol has
        // explicit FLUSH semantics, matching the writeback contract. Must
        // happen before any I/O so first writes hit the fast path.
        let cache = DiskCache::new(raw_fd, read_only);

        // Determine disk geometry
        let file_len = file
            .metadata()
            .expect("nvme: failed to stat backing file")
            .len();
        let block_size: u32 = 512;
        let total_blocks = file_len / u64::from(block_size);

        // Build Identify data structures with unique serial per instance
        let serial = format!("TRITON-NVME-{:04}", instance_id);
        let ctrl_ident = build_identify_controller(
            &serial,
            "Triton VMM NVMe Controller",
            "1.0.0",
            1,  // max_namespaces
            9,  // mdts: 2^9 pages = 2 MiB max transfer (matches C bhyve)
            MAX_IO_QUEUES as u16,
        );
        let ns_ident = build_identify_namespace(total_blocks, block_size);

        // Build CAP register value:
        // MQES = MAX_QUEUE_SIZE - 1 (0-based)
        // CQR = 1 (contiguous queues required)
        // TO = 1 (500 ms timeout)
        // DSTRD = 0 (4-byte doorbell stride)
        // CSS = NVM command set
        // MPSMIN = 0 (4 KiB)
        // MPSMAX = 0 (4 KiB)
        let cap: u64 = (u64::from(MAX_QUEUE_SIZE - 1) << CAP_MQES_SHIFT)
            | CAP_CQR
            | (1u64 << CAP_TO_SHIFT)
            | (0u64 << CAP_DSTRD_SHIFT)
            | CAP_CSS_NVM
            | (0u64 << CAP_MPSMIN_SHIFT)
            | (0u64 << CAP_MPSMAX_SHIFT);

        // PCI identity for NVMe
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
        // BAR0: 32-bit MMIO, 16 KiB for registers + doorbells
        // Using 32-bit to ensure the firmware places it in the
        // 3-4 GB MMIO gap where EPT traps reliably.
        pci_state.define_bar(BarN::BAR0, BarDefine::Mmio(BAR0_SIZE as u32));
        // BAR0 consumes BAR0+BAR1 (64-bit), so BAR4 for MSI-X table

        // Create MSI-X table
        let msix = Arc::new(MsixTable::new(NVME_MSIX_COUNT, Arc::clone(&hdl)));
        let msix_bar_size = msix.bar_size() as u32;

        // BAR4: MMIO for MSI-X table + PBA
        pci_state.define_bar(
            BarN::BAR4,
            BarDefine::Mmio(msix_bar_size.next_power_of_two().max(4096)),
        );

        // Set capabilities pointer to our MSI-X cap
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
            block_size,
            total_blocks,
        };

        // Create the I/O submission channel
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
            _cache: cache,
            _file: file,
            indicator: Indicator::new(),
            gate: Arc::clone(&gate),
        });
        // Store weak self-reference for MMIO closures
        *ctrl.self_ref.lock().expect("self_ref lock") =
            Some(Arc::downgrade(&ctrl));

        // Spawn I/O worker threads
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..NUM_WORKERS {
            let rx = Arc::clone(&rx);
            let physmap = Arc::clone(&physmap);
            let msix = Arc::clone(&msix);
            let state_ref = Arc::clone(&ctrl);
            let gate = Arc::clone(&gate);

            thread::Builder::new()
                .name(format!("nvme-io-{}", i))
                .spawn(move || {
                    io_worker(
                        rx, raw_fd, read_only, physmap, msix, state_ref,
                        gate,
                    );
                })
                .expect("nvme: failed to spawn I/O worker thread");
        }

        ctrl
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
        if let Some(dword_val) = self.msix.cap_read(dword_offset, MSIX_CAP_OFFSET) {
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
            let mut pci =
                self.pci_state.lock().expect("nvme: pci_state lock");
            pci.cfg_write(offset, len, val);

            // Check if BAR or command register changed
            let dword_off = offset & 0xFC;
            if dword_off == 0x04 || (0x10..=0x24).contains(&dword_off) {
                let weak = self.self_ref.lock().expect("self_ref lock");
                if let Some(arc_self) = weak.as_ref().and_then(|w| w.upgrade()) {
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
        if let Some(cur_dword) = self.msix.cap_read(dword_offset, MSIX_CAP_OFFSET) {
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
}

// ---------------------------------------------------------------------------
// BAR0 handling: controller registers + doorbells
// ---------------------------------------------------------------------------

impl NvmeController {
    /// Update MMIO bus registration for BAR0 and BAR4.
    fn update_mmio_registration(self: &Arc<Self>) {
        let pci = self.pci_state.lock().expect("nvme: pci lock");
        let mmio_enabled = pci.command().contains(crate::pci::bits::RegCmd::MMIO_EN);

        // Register both BAR0 and BAR4 on the MMIO bus
        for (bar_n, reg_mutex) in [
            (BarN::BAR0, &self.registered_bar0),
            (BarN::BAR4, &self.registered_bar4),
        ] {
            let mut reg = reg_mutex.lock().expect("bar reg lock");
            if let Some(old_addr) = reg.take() {
                let _ = self.bus_mmio.unregister(old_addr);
            }
            if mmio_enabled {
                if let Some((def, addr)) = pci.bars().get(bar_n) {
                    if addr != 0 && def.is_mmio() {
                        let size = def.size();
                        let dev = Arc::clone(self);
                        let bar = bar_n;
                        let handler: Arc<MmioFn> =
                            Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                                dev.bar_rw(bar, offset, rwo);
                            });
                        if self.bus_mmio.register(addr, size, handler).is_ok() {
                            *reg = Some(addr);
                        }
                    }
                }
            }
        }
    }

    /// Handle BAR0 MMIO access (controller registers and doorbells).
    fn bar0_rw(&self, offset: usize, rwo: RWOp<'_>) {
        if offset < REG_DOORBELL_BASE {
            self.reg_rw(offset, rwo);
        } else {
            self.doorbell_rw(offset, rwo);
        }
    }

    /// Handle controller register reads/writes (offsets 0x00..0x38).
    fn reg_rw(&self, offset: usize, rwo: RWOp<'_>) {
        let mut st = self.state.lock().expect("nvme: state lock");
        match rwo {
            RWOp::Read(ro) => {
                let val = match offset {
                    REG_CAP => {
                        // 8-byte register; handle 4-byte aligned reads
                        if ro.len() == 8 {
                            ro.write_u64(st.cap);
                            return;
                        }
                        st.cap as u32
                    }
                    0x04 => (st.cap >> 32) as u32, // CAP high dword
                    REG_VS => NVME_VS_1_0,
                    REG_INTMS => 0, // unused with MSI-X
                    REG_INTMC => 0,
                    REG_CC => st.cc,
                    REG_CSTS => st.csts,
                    REG_AQA => st.aqa,
                    REG_ASQ => {
                        if ro.len() == 8 {
                            ro.write_u64(st.asq_base);
                            return;
                        }
                        st.asq_base as u32
                    }
                    0x2C => (st.asq_base >> 32) as u32,
                    REG_ACQ => {
                        if ro.len() == 8 {
                            ro.write_u64(st.acq_base);
                            return;
                        }
                        st.acq_base as u32
                    }
                    0x34 => (st.acq_base >> 32) as u32,
                    _ => 0,
                };
                match ro.len() {
                    1 => ro.write_u8(val as u8),
                    2 => ro.write_u16(val as u16),
                    4 => ro.write_u32(val),
                    8 => ro.write_u64(val as u64),
                    _ => ro.write_u32(val),
                }
            }
            RWOp::Write(wo) => {
                let val32 = wo.read_u32();
                match offset {
                    REG_CC => {
                        let old_cc = st.cc;
                        st.cc = val32;
                        self.handle_cc_write(&mut st, old_cc, val32);
                    }
                    REG_AQA => {
                        if (st.cc & CC_EN) == 0 {
                            st.aqa = val32;
                        }
                    }
                    REG_ASQ => {
                        if (st.cc & CC_EN) == 0 {
                            if wo.len() == 8 {
                                st.asq_base = wo.read_u64();
                            } else {
                                st.asq_base =
                                    (st.asq_base & 0xFFFF_FFFF_0000_0000)
                                        | u64::from(val32);
                            }
                        }
                    }
                    0x2C => {
                        if (st.cc & CC_EN) == 0 {
                            st.asq_base = (st.asq_base & 0x0000_0000_FFFF_FFFF)
                                | (u64::from(val32) << 32);
                        }
                    }
                    REG_ACQ => {
                        if (st.cc & CC_EN) == 0 {
                            if wo.len() == 8 {
                                st.acq_base = wo.read_u64();
                            } else {
                                st.acq_base =
                                    (st.acq_base & 0xFFFF_FFFF_0000_0000)
                                        | u64::from(val32);
                            }
                        }
                    }
                    0x34 => {
                        if (st.cc & CC_EN) == 0 {
                            st.acq_base =
                                (st.acq_base & 0x0000_0000_FFFF_FFFF)
                                    | (u64::from(val32) << 32);
                        }
                    }
                    // INTMS, INTMC: ignored (MSI-X mode)
                    _ => {}
                }
            }
        }
    }

    /// Handle CC register write transitions (enable/disable/shutdown).
    fn handle_cc_write(
        &self,
        st: &mut NvmeState,
        old_cc: u32,
        new_cc: u32,
    ) {
        let was_enabled = (old_cc & CC_EN) != 0;
        let now_enabled = (new_cc & CC_EN) != 0;

        if !was_enabled && now_enabled {
            // Controller enable: set up admin queues
            let asqs = (st.aqa & AQA_ASQS_MASK) + 1;
            let acqs = ((st.aqa >> AQA_ACQS_SHIFT) & AQA_ACQS_MASK) + 1;

            st.admin_sq = Some(SubQueue {
                base: st.asq_base,
                size: asqs as u16,
                head: 0,
                tail: 0,
                cq_id: 0,
            });
            st.admin_cq = Some(CompQueue {
                base: st.acq_base,
                size: acqs as u16,
                head: 0,
                tail: 0,
                phase: true,
                iv: 0,
            });

            st.csts |= CSTS_RDY;
        } else if was_enabled && !now_enabled {
            // Controller disable: tear down all queues
            st.admin_sq = None;
            st.admin_cq = None;
            for sq in &mut st.io_sqs {
                *sq = None;
            }
            for cq in &mut st.io_cqs {
                *cq = None;
            }
            st.csts &= !CSTS_RDY;
        }

        // Handle shutdown notification
        let shn = (new_cc >> CC_SHN_SHIFT) & CC_SHN_MASK;
        if shn != SHST_NORMAL {
            // Acknowledge shutdown complete
            st.csts = (st.csts & !(CSTS_SHST_MASK << CSTS_SHST_SHIFT))
                | (SHST_COMPLETE << CSTS_SHST_SHIFT);
        }
    }

    /// Handle doorbell writes (offsets 0x1000+).
    ///
    /// Doorbell layout (DSTRD=0, 4-byte stride):
    /// - 0x1000 + (2*qid + 0) * 4 = SQ tail doorbell for queue qid
    /// - 0x1000 + (2*qid + 1) * 4 = CQ head doorbell for queue qid
    #[inline]
    fn doorbell_rw(&self, offset: usize, rwo: RWOp<'_>) {
        // Doorbells are write-only; reads return 0
        let wo = match rwo {
            RWOp::Write(wo) => wo,
            RWOp::Read(ro) => {
                ro.write_u32(0);
                return;
            }
        };

        let db_offset = offset - REG_DOORBELL_BASE;
        let db_index = db_offset / 4;
        let qid = (db_index / 2) as u16;
        let is_sq = (db_index % 2) == 0;
        let new_val = wo.read_u32() as u16;

        if is_sq {
            self.sq_doorbell(qid, new_val);
        } else {
            self.cq_doorbell(qid, new_val);
        }
    }

    /// Handle SQ tail doorbell write: process new submissions.
    #[inline]
    fn sq_doorbell(&self, qid: u16, new_tail: u16) {
        if qid == ADMIN_QUEUE_ID {
            self.admin_sq_doorbell(new_tail);
        } else {
            self.io_sq_doorbell(qid, new_tail);
        }
    }

    /// Handle CQ head doorbell write: update head pointer.
    #[inline]
    fn cq_doorbell(&self, qid: u16, new_head: u16) {
        let mut st = self.state.lock().expect("nvme: state lock");
        if qid == ADMIN_QUEUE_ID {
            if let Some(cq) = &mut st.admin_cq {
                if new_head < cq.size {
                    cq.head = new_head;
                }
            }
        } else {
            let idx = (qid - 1) as usize;
            if idx < MAX_IO_QUEUES {
                if let Some(cq) = &mut st.io_cqs[idx] {
                    if new_head < cq.size {
                        cq.head = new_head;
                    }
                }
            }
        }
    }

    /// Process admin SQ doorbell: parse and execute admin commands inline.
    fn admin_sq_doorbell(&self, new_tail: u16) {
        let mem = MemCtx::new(Arc::clone(&self.physmap));
        let mut st = self.state.lock().expect("nvme: state lock");

        // Update the tail pointer (validate against queue size)
        match &mut st.admin_sq {
            Some(sq) => {
                if new_tail >= sq.size {
                    return;
                }
                sq.tail = new_tail;
            }
            None => return,
        }

        // Process entries in a loop, re-borrowing each iteration to
        // avoid holding a mutable borrow on admin_sq across the call
        // to process_admin_cmd (which borrows the full NvmeState).
        loop {
            // Read the next SQE from guest memory (borrow admin_sq briefly)
            let (sqe_gpa, sq_head, sq_size) = {
                let sq = match &st.admin_sq {
                    Some(sq) => sq,
                    None => return,
                };
                if sq.head == sq.tail {
                    break;
                }
                let sqe_offset = u64::from(sq.head) * SQE_SIZE as u64;
                (sq.base + sqe_offset, sq.head, sq.size)
            };

            let mut sqe_buf = [0u8; SQE_SIZE];
            if mem.read(sqe_gpa, &mut sqe_buf).is_err() {
                // Advance head past the bad entry
                if let Some(sq) = &mut st.admin_sq {
                    sq.head = (sq.head + 1) % sq.size;
                }
                continue;
            }

            let opcode = sqe_buf[SQE_OPC];
            let cid = u16::from_le_bytes(
                [sqe_buf[SQE_CID], sqe_buf[SQE_CID + 1]],
            );
            let nsid = u32::from_le_bytes(
                sqe_buf[SQE_NSID..SQE_NSID + 4]
                    .try_into()
                    .expect("4 bytes"),
            );
            let prp1 = u64::from_le_bytes(
                sqe_buf[SQE_PRP1..SQE_PRP1 + 8]
                    .try_into()
                    .expect("8 bytes"),
            );
            let prp2 = u64::from_le_bytes(
                sqe_buf[SQE_PRP2..SQE_PRP2 + 8]
                    .try_into()
                    .expect("8 bytes"),
            );
            let cdw10 = u32::from_le_bytes(
                sqe_buf[SQE_CDW10..SQE_CDW10 + 4]
                    .try_into()
                    .expect("4 bytes"),
            );
            let cdw11 = u32::from_le_bytes(
                sqe_buf[SQE_CDW11..SQE_CDW11 + 4]
                    .try_into()
                    .expect("4 bytes"),
            );

            // Advance the SQ head
            if let Some(sq) = &mut st.admin_sq {
                sq.head = (sq_head + 1) % sq_size;
            }

            // Now process the command with full NvmeState access
            let (status, cdw0) = self.process_admin_cmd(
                &mut st,
                &mem,
                opcode,
                cid,
                nsid,
                prp1,
                prp2,
                cdw10,
                cdw11,
            );

            // Write CQE to admin CQ
            post_completion(
                &mem,
                &self.msix,
                &mut st.admin_cq,
                cdw0,
                ADMIN_QUEUE_ID,
                sq_head,
                cid,
                status,
            );
        }
    }

    /// Process a single admin command. Returns (status, cdw0_result).
    fn process_admin_cmd(
        &self,
        st: &mut NvmeState,
        mem: &MemCtx,
        opcode: u8,
        cid: u16,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let _ = cid;
        let result = match opcode {
            ADMIN_OPC_IDENTIFY => {
                self.admin_identify(st, mem, nsid, prp1, prp2, cdw10)
            }
            ADMIN_OPC_CREATE_IO_CQ => {
                self.admin_create_io_cq(st, prp1, cdw10, cdw11)
            }
            ADMIN_OPC_CREATE_IO_SQ => {
                self.admin_create_io_sq(st, prp1, cdw10, cdw11)
            }
            ADMIN_OPC_DELETE_IO_SQ => {
                self.admin_delete_io_sq(st, cdw10)
            }
            ADMIN_OPC_DELETE_IO_CQ => {
                self.admin_delete_io_cq(st, cdw10)
            }
            ADMIN_OPC_GET_FEATURES => {
                self.admin_get_features(st, cdw10)
            }
            ADMIN_OPC_SET_FEATURES => {
                self.admin_set_features(st, cdw10, cdw11)
            }
            ADMIN_OPC_ABORT => {
                // Abort: always report "command not found" (success).
                // CDW0 bit 0 = 0 means the command was not found (no abort).
                (SC_SUCCESS, 0)
            }
            ADMIN_OPC_GET_LOG_PAGE => {
                // Minimal stub: return zeros
                let len = ((cdw10 >> 16) as usize + 1) * 4;
                let zeros = vec![0u8; len.min(4096)];
                let _ = mem.write(prp1, &zeros);
                (SC_SUCCESS, 0)
            }
            _ => (SC_INVALID_OPCODE | (SC_DNR), 0),
        };
        result
    }

    /// Identify command (CNS 0 = namespace, CNS 1 = controller).
    fn admin_identify(
        &self,
        st: &NvmeState,
        mem: &MemCtx,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
    ) -> (u16, u32) {
        let cns = (cdw10 & 0xFF) as u8;
        let data: &[u8] = match cns {
            IDENTIFY_CNS_NAMESPACE => {
                if nsid != 1 {
                    return (SC_INVALID_NS | SC_DNR, 0);
                }
                &st.ns_ident
            }
            IDENTIFY_CNS_CONTROLLER => &st.ctrl_ident,
            _ => return (SC_INVALID_FIELD | SC_DNR, 0),
        };

        // Write 4096 bytes to guest memory via PRP
        if write_to_prp(mem, prp1, prp2, data).is_err() {
            return (SC_DATA_XFER_ERROR, 0);
        }
        (SC_SUCCESS, 0)
    }

    /// Create I/O Completion Queue.
    fn admin_create_io_cq(
        &self,
        st: &mut NvmeState,
        prp1: u64,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        let qsize = ((cdw10 >> 16) & 0xFFFF) as u16 + 1; // Convert 0-based to 1-based
        let iv = ((cdw11 >> 16) & 0xFFFF) as u16;

        if qid == 0 || qid as usize > MAX_IO_QUEUES {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        if qsize < 2 || qsize > MAX_QUEUE_SIZE {
            return (SC_INVALID_QUEUE_SIZE | SC_DNR, 0);
        }

        let idx = (qid - 1) as usize;
        if st.io_cqs[idx].is_some() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }

        // Validate that the queue base GPA is page-aligned and non-zero.
        // A malicious guest could point CQ base at an MMIO region of
        // another device, causing cross-device confusion.
        if prp1 == 0 || (prp1 & 0xFFF) != 0 {
            return (SC_INVALID_FIELD | SC_DNR, 0);
        }

        st.io_cqs[idx] = Some(CompQueue {
            base: prp1,
            size: qsize,
            head: 0,
            tail: 0,
            phase: true,
            iv,
        });

        (SC_SUCCESS, 0)
    }

    /// Create I/O Submission Queue.
    fn admin_create_io_sq(
        &self,
        st: &mut NvmeState,
        prp1: u64,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        let qsize = ((cdw10 >> 16) & 0xFFFF) as u16 + 1;
        let cq_id = ((cdw11 >> 16) & 0xFFFF) as u16;

        if qid == 0 || qid as usize > MAX_IO_QUEUES {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        if qsize < 2 || qsize > MAX_QUEUE_SIZE {
            return (SC_INVALID_QUEUE_SIZE | SC_DNR, 0);
        }

        let idx = (qid - 1) as usize;
        if st.io_sqs[idx].is_some() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }

        // Verify the associated CQ exists
        let cq_idx = (cq_id - 1) as usize;
        if cq_id == 0 || cq_idx >= MAX_IO_QUEUES || st.io_cqs[cq_idx].is_none()
        {
            return (SC_INVALID_FIELD | SC_DNR, 0);
        }

        // Validate that the queue base GPA is page-aligned and non-zero.
        if prp1 == 0 || (prp1 & 0xFFF) != 0 {
            return (SC_INVALID_FIELD | SC_DNR, 0);
        }

        st.io_sqs[idx] = Some(SubQueue {
            base: prp1,
            size: qsize,
            head: 0,
            tail: 0,
            cq_id,
        });

        (SC_SUCCESS, 0)
    }

    /// Delete I/O Submission Queue.
    fn admin_delete_io_sq(
        &self,
        st: &mut NvmeState,
        cdw10: u32,
    ) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        if qid == 0 || qid as usize > MAX_IO_QUEUES {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        let idx = (qid - 1) as usize;
        if st.io_sqs[idx].is_none() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        st.io_sqs[idx] = None;
        (SC_SUCCESS, 0)
    }

    /// Delete I/O Completion Queue.
    fn admin_delete_io_cq(
        &self,
        st: &mut NvmeState,
        cdw10: u32,
    ) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        if qid == 0 || qid as usize > MAX_IO_QUEUES {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        let idx = (qid - 1) as usize;
        if st.io_cqs[idx].is_none() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        // Verify no SQs reference this CQ
        for sq in &st.io_sqs {
            if let Some(sq) = sq {
                if sq.cq_id == qid {
                    return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
                }
            }
        }
        st.io_cqs[idx] = None;
        (SC_SUCCESS, 0)
    }

    /// Get Features command.
    fn admin_get_features(
        &self,
        st: &NvmeState,
        cdw10: u32,
    ) -> (u16, u32) {
        let fid = (cdw10 & 0xFF) as u8;
        match fid {
            FEAT_NUM_QUEUES => {
                // CDW0 bits 31:16 = num CQs allocated (0-based)
                // CDW0 bits 15:0  = num SQs allocated (0-based)
                let nq = u32::from(st.num_io_queues - 1);
                (SC_SUCCESS, nq | (nq << 16))
            }
            FEAT_VOLATILE_WC => {
                // Volatile write cache: report enabled
                (SC_SUCCESS, 1)
            }
            FEAT_TEMP_THRESHOLD
            | FEAT_ERROR_RECOVERY
            | FEAT_POWER_MGMT
            | FEAT_ARBITRATION
            | FEAT_INTR_COALESCING
            | FEAT_INTR_VECTOR_CFG
            | FEAT_WRITE_ATOMICITY
            | FEAT_ASYNC_EVENT_CFG => (SC_SUCCESS, 0),
            _ => (SC_INVALID_FIELD | SC_DNR, 0),
        }
    }

    /// Set Features command.
    fn admin_set_features(
        &self,
        st: &mut NvmeState,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let fid = (cdw10 & 0xFF) as u8;
        match fid {
            FEAT_NUM_QUEUES => {
                // Guest requests N SQs and N CQs (0-based in CDW11)
                let requested_sq = (cdw11 & 0xFFFF) as u16 + 1;
                let requested_cq = ((cdw11 >> 16) & 0xFFFF) as u16 + 1;
                let max = MAX_IO_QUEUES as u16;
                let alloc_sq = requested_sq.min(max);
                let alloc_cq = requested_cq.min(max);
                st.num_io_queues = alloc_sq.min(alloc_cq);
                let result_sq = u32::from(alloc_sq - 1);
                let result_cq = u32::from(alloc_cq - 1);
                (SC_SUCCESS, result_sq | (result_cq << 16))
            }
            FEAT_VOLATILE_WC => (SC_SUCCESS, 0),
            FEAT_TEMP_THRESHOLD
            | FEAT_ERROR_RECOVERY
            | FEAT_POWER_MGMT
            | FEAT_ARBITRATION
            | FEAT_INTR_COALESCING
            | FEAT_INTR_VECTOR_CFG
            | FEAT_WRITE_ATOMICITY
            | FEAT_ASYNC_EVENT_CFG => (SC_SUCCESS, 0),
            _ => (SC_INVALID_FIELD | SC_DNR, 0),
        }
    }

    /// Process I/O SQ doorbell: parse SQEs and submit to worker channel.
    fn io_sq_doorbell(&self, qid: u16, new_tail: u16) {
        let mem = MemCtx::new(Arc::clone(&self.physmap));
        let mut st = self.state.lock().expect("nvme: state lock");

        let idx = (qid - 1) as usize;
        if idx >= MAX_IO_QUEUES {
            return;
        }

        let sq = match &mut st.io_sqs[idx] {
            Some(sq) => sq,
            None => return,
        };

        let cq_id = sq.cq_id;
        if new_tail >= sq.size {
            return;
        }
        sq.tail = new_tail;

        // Collect all new SQEs before releasing the lock
        let mut requests = Vec::new();

        while sq.head != sq.tail {
            let sqe_offset = u64::from(sq.head) * SQE_SIZE as u64;
            let sqe_gpa = sq.base + sqe_offset;

            let mut sqe_buf = [0u8; SQE_SIZE];
            if mem.read(sqe_gpa, &mut sqe_buf).is_err() {
                sq.head = (sq.head + 1) % sq.size;
                continue;
            }

            let opcode = sqe_buf[SQE_OPC];
            let cid = u16::from_le_bytes(
                [sqe_buf[SQE_CID], sqe_buf[SQE_CID + 1]],
            );
            let nsid = u32::from_le_bytes(
                sqe_buf[SQE_NSID..SQE_NSID + 4]
                    .try_into()
                    .expect("4 bytes"),
            );
            let prp1 = u64::from_le_bytes(
                sqe_buf[SQE_PRP1..SQE_PRP1 + 8]
                    .try_into()
                    .expect("8 bytes"),
            );
            let prp2 = u64::from_le_bytes(
                sqe_buf[SQE_PRP2..SQE_PRP2 + 8]
                    .try_into()
                    .expect("8 bytes"),
            );
            let cdw10 = u32::from_le_bytes(
                sqe_buf[SQE_CDW10..SQE_CDW10 + 4]
                    .try_into()
                    .expect("4 bytes"),
            );
            let cdw12 = u32::from_le_bytes(
                sqe_buf[SQE_CDW12..SQE_CDW12 + 4]
                    .try_into()
                    .expect("4 bytes"),
            );

            // For NVM Read/Write: CDW10-11 = SLBA (64-bit), CDW12 bits 15:0 = NLB (0-based)
            let slba = u64::from(cdw10)
                | (u64::from(u32::from_le_bytes(
                    sqe_buf[SQE_CDW11..SQE_CDW11 + 4]
                        .try_into()
                        .expect("4 bytes"),
                )) << 32);
            let nlb = (cdw12 & 0xFFFF) + 1; // Convert 0-based to 1-based
            // NVMe 5.13: CDW12 bit 30 = Force Unit Access.
            let fua = (cdw12 & (1u32 << 30)) != 0;

            sq.head = (sq.head + 1) % sq.size;

            requests.push(IoRequest {
                opcode,
                nsid,
                slba,
                nlb,
                prp1,
                prp2,
                sqid: qid,
                cid,
                cq_id: cq_id,
                fua,
            });
        }

        // Drop the state lock before sending to channel
        drop(st);

        // Send requests to worker threads
        let tx_guard = self.io_tx.lock().expect("nvme: io_tx lock");
        if let Some(tx) = tx_guard.as_ref() {
            for req in requests {
                // If the channel is disconnected, the workers have exited.
                // Silently drop the request.
                let _ = tx.send(req);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BAR4 handling: MSI-X table
// ---------------------------------------------------------------------------

impl NvmeController {
    /// Handle BAR4 MMIO access (MSI-X table + PBA).
    fn bar4_rw(&self, offset: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                let val = self.msix.table_read(offset);
                match ro.len() {
                    1 => ro.write_u8(val as u8),
                    2 => ro.write_u16(val as u16),
                    4 => ro.write_u32(val),
                    _ => ro.write_u32(val),
                }
            }
            RWOp::Write(wo) => {
                self.msix.table_write(offset, wo.read_u32());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Completion queue helpers
// ---------------------------------------------------------------------------

/// Post a completion queue entry to guest memory and fire MSI-X.
fn post_completion(
    mem: &MemCtx,
    msix: &MsixTable,
    cq_opt: &mut Option<CompQueue>,
    cdw0: u32,
    sqid: u16,
    sq_head: u16,
    cid: u16,
    status: u16,
) {
    let cq = match cq_opt.as_mut() {
        Some(cq) => cq,
        None => return,
    };

    // Check if CQ is full: (tail + 1) % size == head
    let next_tail = if cq.tail + 1 >= cq.size { 0 } else { cq.tail + 1 };
    if next_tail == cq.head {
        // CQ full -- drop the completion (guest must drain the queue)
        return;
    }

    let cqe_offset = u64::from(cq.tail) * CQE_SIZE as u64;
    let cqe_gpa = cq.base + cqe_offset;

    // Build CQE (16 bytes)
    let mut cqe = [0u8; CQE_SIZE];

    // DW0: command-specific result
    cqe[CQE_DW0..CQE_DW0 + 4].copy_from_slice(&cdw0.to_le_bytes());
    // DW1: reserved (already zero)

    // DW2: SQ head pointer + SQ ID
    cqe[CQE_SQHD..CQE_SQHD + 2].copy_from_slice(&sq_head.to_le_bytes());
    cqe[CQE_SQID..CQE_SQID + 2].copy_from_slice(&sqid.to_le_bytes());

    // DW3: CID + Status (with phase bit in bit 0 of status field)
    cqe[CQE_CID..CQE_CID + 2].copy_from_slice(&cid.to_le_bytes());

    // Status field: bits 15:1 = status code, bit 0 = phase
    let status_with_phase = (status << 1) | if cq.phase { 1 } else { 0 };
    cqe[CQE_STATUS..CQE_STATUS + 2]
        .copy_from_slice(&status_with_phase.to_le_bytes());

    // Write CQE to guest memory
    let _ = mem.write(cqe_gpa, &cqe);

    // Advance CQ tail and toggle phase on wrap
    let iv = cq.iv;
    cq.tail += 1;
    if cq.tail >= cq.size {
        cq.tail = 0;
        cq.phase = !cq.phase;
    }

    // Fire MSI-X interrupt
    msix.fire(iv);
}

// ---------------------------------------------------------------------------
// PRP handling
// ---------------------------------------------------------------------------

/// Resolve PRP entries into a list of (guest_physical_addr, length) segments.
///
/// For NVMe, PRP entry 1 is the first page (possibly unaligned start),
/// PRP entry 2 is either a second page or a PRP list page for larger
/// transfers.
fn prp_collect(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    total_len: usize,
) -> io::Result<Vec<(u64, usize)>> {
    let mut segments = Vec::new();
    let mut remaining = total_len;

    if remaining == 0 {
        return Ok(segments);
    }

    // PRP1: first page, possibly unaligned
    let offset_in_page = (prp1 as usize) & (PAGE_SIZE - 1);
    let first_len = (PAGE_SIZE - offset_in_page).min(remaining);
    segments.push((prp1, first_len));
    remaining -= first_len;

    if remaining == 0 {
        return Ok(segments);
    }

    // If remaining fits in a single page, PRP2 is a direct page address
    if remaining <= PAGE_SIZE {
        segments.push((prp2, remaining));
        return Ok(segments);
    }

    // PRP2 points to a PRP list page
    // Each entry in the PRP list is an 8-byte physical address
    let max_entries = PAGE_SIZE / 8;
    let list_gpa = prp2;
    let mut entry_idx = 0;

    while remaining > 0 && entry_idx < max_entries {
        let entry_gpa = list_gpa + (entry_idx as u64) * 8;
        let mut addr_buf = [0u8; 8];
        mem.read(entry_gpa, &mut addr_buf)?;
        let page_addr = u64::from_le_bytes(addr_buf);

        let seg_len = PAGE_SIZE.min(remaining);
        segments.push((page_addr, seg_len));
        remaining -= seg_len;
        entry_idx += 1;
    }

    if remaining > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PRP list insufficient for transfer size",
        ));
    }

    Ok(segments)
}

/// Resolve PRP entries into iovecs pointing directly into guest mmap'd memory.
///
/// Returns a Vec of (iovec, SubMapping) pairs. The SubMappings must be
/// kept alive for the duration of the preadv/pwritev call — they pin
/// the guest memory pages in our process address space.
///
/// This is the zero-copy I/O path: no intermediate buffer, no copies.
fn prp_to_iovecs(
    physmap: &PhysMap,
    prp1: u64,
    prp2: u64,
    total_len: usize,
) -> io::Result<Vec<(libc::iovec, SubMapping)>> {
    let mut result = Vec::with_capacity(16);
    let mut remaining = total_len;

    if remaining == 0 {
        return Ok(result);
    }

    // PRP1: first page, possibly unaligned
    let offset_in_page = (prp1 as usize) & (PAGE_SIZE - 1);
    let first_len = (PAGE_SIZE - offset_in_page).min(remaining);
    let sub = physmap.lookup(prp1, first_len).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "PRP1 unmapped")
    })?;
    let iov = libc::iovec {
        iov_base: unsafe { sub.as_ptr() } as *mut libc::c_void,
        iov_len: first_len,
    };
    result.push((iov, sub));
    remaining -= first_len;

    if remaining == 0 {
        return Ok(result);
    }

    // If remaining fits in a single page, PRP2 is a direct page address
    if remaining <= PAGE_SIZE {
        let sub = physmap.lookup(prp2, remaining).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "PRP2 unmapped")
        })?;
        let iov = libc::iovec {
            iov_base: unsafe { sub.as_ptr() } as *mut libc::c_void,
            iov_len: remaining,
        };
        result.push((iov, sub));
        return Ok(result);
    }

    // PRP2 points to a PRP list page — read entries from guest memory
    let max_entries = PAGE_SIZE / 8;
    let mut entry_idx = 0;

    while remaining > 0 && entry_idx < max_entries {
        let entry_gpa = prp2 + (entry_idx as u64) * 8;
        // Read the 8-byte PRP list entry from guest memory
        let entry_sub = physmap.lookup(entry_gpa, 8).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "PRP list unmapped")
        })?;
        let mut addr_buf = [0u8; 8];
        entry_sub.read_bytes(&mut addr_buf)?;
        let page_addr = u64::from_le_bytes(addr_buf);

        let seg_len = PAGE_SIZE.min(remaining);
        let sub = physmap.lookup(page_addr, seg_len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "PRP page unmapped")
        })?;
        let iov = libc::iovec {
            iov_base: unsafe { sub.as_ptr() } as *mut libc::c_void,
            iov_len: seg_len,
        };
        result.push((iov, sub));
        remaining -= seg_len;
        entry_idx += 1;
    }

    if remaining > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PRP list insufficient for transfer size",
        ));
    }

    Ok(result)
}

/// Write data to guest memory using PRP entries (for Identify etc.).
fn write_to_prp(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    data: &[u8],
) -> io::Result<()> {
    let segments = prp_collect(mem, prp1, prp2, data.len())?;
    let mut offset = 0;
    for (gpa, len) in segments {
        mem.write(gpa, &data[offset..offset + len])?;
        offset += len;
    }
    Ok(())
}

/// Read data from guest memory using PRP entries.
fn read_from_prp(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    buf: &mut [u8],
) -> io::Result<()> {
    let segments = prp_collect(mem, prp1, prp2, buf.len())?;
    let mut offset = 0;
    for (gpa, len) in segments {
        mem.read(gpa, &mut buf[offset..offset + len])?;
        offset += len;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// I/O worker thread
// ---------------------------------------------------------------------------

/// Worker thread loop: reads I/O requests from the channel and executes them.
///
/// Uses `pread`/`pwrite` on the raw fd for lock-free concurrent I/O.
/// Each completed request posts a CQE to guest memory and fires MSI-X.
fn io_worker(
    rx: Arc<Mutex<mpsc::Receiver<IoRequest>>>,
    raw_fd: i32,
    read_only: bool,
    physmap: Arc<PhysMap>,
    msix: Arc<MsixTable>,
    ctrl: Arc<NvmeController>,
    gate: Arc<QuiesceGate>,
) {
    use std::sync::mpsc::RecvTimeoutError;

    loop {
        // Acquire the receiver lock only to get the next message
        let req = {
            let rx_guard = rx.lock().expect("nvme: io_rx lock");
            match rx_guard.recv_timeout(Duration::from_millis(50)) {
                Ok(req) => req,
                Err(RecvTimeoutError::Timeout) => {
                    drop(rx_guard);
                    gate.park_if_paused();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };

        let status = process_io_request(
            &req,
            raw_fd,
            read_only,
            &physmap,
            &ctrl,
        );
        // Post completion: need to lock state briefly to access the CQ
        let mem = MemCtx::new(Arc::clone(&physmap));
        let sq_head = {
            let st = ctrl.state.lock().expect("nvme: state lock");
            let idx = (req.sqid - 1) as usize;
            if idx < MAX_IO_QUEUES {
                st.io_sqs[idx].as_ref().map(|sq| sq.head).unwrap_or(0)
            } else {
                0
            }
        };

        let mut st = ctrl.state.lock().expect("nvme: state lock");
        let cq_idx = (req.cq_id - 1) as usize;
        if cq_idx < MAX_IO_QUEUES {
            post_completion(
                &mem,
                &msix,
                &mut st.io_cqs[cq_idx],
                0,
                req.sqid,
                sq_head,
                req.cid,
                status,
            );
        }
    }
}

/// Execute a single I/O request (Read, Write, or Flush).
///
/// Returns the NVMe status code for the completion entry.
fn process_io_request(
    req: &IoRequest,
    raw_fd: i32,
    read_only: bool,
    physmap: &Arc<PhysMap>,
    ctrl: &NvmeController,
) -> u16 {
    // Validate namespace
    if req.nsid != 1 {
        return SC_INVALID_NS | SC_DNR;
    }

    let (block_size, total_blocks) = {
        let st = ctrl.state.lock().expect("nvme: state lock");
        (st.block_size, st.total_blocks)
    };

    let byte_count = match (req.nlb as u64).checked_mul(u64::from(block_size)) {
        Some(n) => n as usize,
        None => return SC_INVALID_FIELD | SC_DNR,
    };
    // MDTS check: 2^9 pages = 2 MiB max transfer
    const MAX_XFER_SIZE: usize = (1 << 9) * 4096;
    if byte_count > MAX_XFER_SIZE {
        return SC_INVALID_FIELD | SC_DNR;
    }
    let byte_offset = match req.slba.checked_mul(u64::from(block_size)) {
        Some(n) => n,
        None => return SC_INVALID_FIELD | SC_DNR,
    };

    // Bounds check (overflow in addition means out of bounds)
    if req.slba.checked_add(u64::from(req.nlb)).map_or(true, |end| end > total_blocks) {
        return SC_INVALID_FIELD | SC_DNR;
    }

    match req.opcode {
        NVM_OPC_READ => {
            // Zero-copy: preadv directly into guest mmap'd pages
            let iovecs_subs = match prp_to_iovecs(physmap, req.prp1, req.prp2, byte_count) {
                Ok(v) => v,
                Err(_) => return SC_DATA_XFER_ERROR,
            };
            let iovecs: Vec<libc::iovec> = iovecs_subs.iter().map(|(iov, _)| *iov).collect();
            let nread = unsafe {
                libc::preadv(
                    raw_fd,
                    iovecs.as_ptr(),
                    iovecs.len() as libc::c_int,
                    byte_offset as libc::off_t,
                )
            };
            // SubMappings in iovecs_subs keep guest pages pinned until here
            drop(iovecs_subs);
            if nread < 0 || nread as usize != byte_count {
                return SC_DATA_XFER_ERROR;
            }
            SC_SUCCESS
        }
        NVM_OPC_WRITE => {
            if read_only {
                return SC_DATA_XFER_ERROR;
            }
            // Zero-copy: pwritev directly from guest mmap'd pages
            let iovecs_subs = match prp_to_iovecs(physmap, req.prp1, req.prp2, byte_count) {
                Ok(v) => v,
                Err(_) => return SC_DATA_XFER_ERROR,
            };
            let iovecs: Vec<libc::iovec> = iovecs_subs.iter().map(|(iov, _)| *iov).collect();
            let nwritten = unsafe {
                libc::pwritev(
                    raw_fd,
                    iovecs.as_ptr(),
                    iovecs.len() as libc::c_int,
                    byte_offset as libc::off_t,
                )
            };
            drop(iovecs_subs);
            if nwritten < 0 || nwritten as usize != byte_count {
                return SC_DATA_XFER_ERROR;
            }
            // FUA: Identify Controller advertises VWC=1, so FUA writes
            // must be durable before completion. Without this, a guest
            // filesystem journal commit can report success while the data
            // still lives in zvol writeback cache.
            if req.fua && datasync(raw_fd) != 0 {
                return SC_INTERNAL_ERROR;
            }
            SC_SUCCESS
        }
        NVM_OPC_FLUSH => {
            if datasync(raw_fd) != 0 {
                return SC_INTERNAL_ERROR;
            }
            SC_SUCCESS
        }
        _ => SC_INVALID_OPCODE | SC_DNR,
    }
}

// ---------------------------------------------------------------------------
// Default impl for Option arrays
// ---------------------------------------------------------------------------

// SubQueue and CompQueue don't implement Copy/Clone, so we need a manual
// Default for the fixed-size arrays.
impl Default for NvmeState {
    fn default() -> Self {
        // This is only used internally; the real init is in NvmeController::new
        Self {
            cap: 0,
            cc: 0,
            csts: 0,
            aqa: 0,
            asq_base: 0,
            acq_base: 0,
            admin_sq: None,
            admin_cq: None,
            io_sqs: Default::default(),
            io_cqs: Default::default(),
            ctrl_ident: [0u8; IDENT_CTRL_SIZE],
            ns_ident: [0u8; IDENT_NS_SIZE],
            num_io_queues: 0,
            block_size: 512,
            total_blocks: 0,
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

    fn lifecycle_state(&self) -> Option<crate::lifecycle::IndicatedState> {
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
    }

    fn flush_backing(&self, intent: FlushIntent) -> Result<(), FlushError> {
        crate::quiesce::flush_file(&self.gate, &self._file, intent, "nvme")
    }

    fn export_nvme_state(
        &self,
    ) -> Option<crate::lifecycle::NvmeMigrateState> {
        use crate::lifecycle::{NvmeMigrateCq, NvmeMigrateSq, NvmeMigrateState};

        let st = self.state.lock().expect("nvme state lock");

        // Only export if the controller is enabled (has active queues)
        if (st.cc & CC_EN) == 0 {
            return None;
        }

        let admin_sq = st.admin_sq.as_ref().map(|sq| NvmeMigrateSq {
            base: sq.base, size: sq.size, head: sq.head,
            tail: sq.tail, cq_id: sq.cq_id,
        });
        let admin_cq = st.admin_cq.as_ref().map(|cq| NvmeMigrateCq {
            base: cq.base, size: cq.size, head: cq.head,
            tail: cq.tail, phase: cq.phase, iv: cq.iv,
        });
        let io_sqs = st.io_sqs.iter().map(|sq| {
            sq.as_ref().map(|sq| NvmeMigrateSq {
                base: sq.base, size: sq.size, head: sq.head,
                tail: sq.tail, cq_id: sq.cq_id,
            })
        }).collect();
        let io_cqs = st.io_cqs.iter().map(|cq| {
            cq.as_ref().map(|cq| NvmeMigrateCq {
                base: cq.base, size: cq.size, head: cq.head,
                tail: cq.tail, phase: cq.phase, iv: cq.iv,
            })
        }).collect();

        // Export PCI state
        let pci = self.pci_state.lock().expect("pci lock");
        let mut pci_bar_addrs = [0u64; 6];
        for (idx, addr) in pci_bar_addrs.iter_mut().enumerate() {
            let bar = BarN::try_from(idx as u8).expect("valid BAR");
            *addr = pci.bars().get(bar).map(|(_, v)| v).unwrap_or(0);
        }
        let pci_command = pci.command().bits();
        drop(pci);

        // Export MSI-X table entries
        let msix_count = self.msix.count();
        let msix_entries: Vec<(u64, u64)> = (0..msix_count)
            .map(|v| self.msix.read_entry(v))
            .collect();
        let msix_enabled = self.msix.is_enabled();

        eprintln!(
            "[nvme-export] cc={:#x} csts={:#x} aqa={:#x} io_queues={} msix_enabled={}",
            st.cc, st.csts, st.aqa, st.num_io_queues, msix_enabled,
        );

        Some(NvmeMigrateState {
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
        })
    }

    fn restore_nvme_state(
        &self,
        mig: &crate::lifecycle::NvmeMigrateState,
    ) {
        eprintln!(
            "[nvme-restore] cc={:#x} csts={:#x} aqa={:#x} io_queues={} msix_enabled={}",
            mig.cc, mig.csts, mig.aqa, mig.num_io_queues, mig.msix_enabled,
        );

        // Restore PCI BAR addresses and command register
        {
            use crate::pci::bar::BarDefine;
            let pci = self.pci_state.lock().expect("pci lock");
            let mut bar_writes = Vec::new();
            for (idx, &addr) in mig.pci_bar_addrs.iter().enumerate() {
                let bar = BarN::try_from(idx as u8).expect("valid BAR");
                let Some((def, _)) = pci.bars().get(bar) else { continue };
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
            drop(pci);
            for (off, val) in &bar_writes {
                self.cfg_write(*off, 4, *val);
            }
            self.cfg_write(
                crate::pci::bits::REG_COMMAND, 2, u32::from(mig.pci_command),
            );
        }

        // Restore controller state
        let mut st = self.state.lock().expect("nvme state lock");
        st.cc = mig.cc;
        st.csts = mig.csts;
        st.aqa = mig.aqa;
        st.asq_base = mig.asq_base;
        st.acq_base = mig.acq_base;
        st.num_io_queues = mig.num_io_queues;

        // Restore admin queues
        st.admin_sq = mig.admin_sq.as_ref().map(|sq| SubQueue {
            base: sq.base, size: sq.size, head: sq.head,
            tail: sq.tail, cq_id: sq.cq_id,
        });
        st.admin_cq = mig.admin_cq.as_ref().map(|cq| CompQueue {
            base: cq.base, size: cq.size, head: cq.head,
            tail: cq.tail, phase: cq.phase, iv: cq.iv,
        });

        // Restore I/O queues
        for (idx, sq_opt) in mig.io_sqs.iter().enumerate() {
            if idx < MAX_IO_QUEUES {
                st.io_sqs[idx] = sq_opt.as_ref().map(|sq| SubQueue {
                    base: sq.base, size: sq.size, head: sq.head,
                    tail: sq.tail, cq_id: sq.cq_id,
                });
            }
        }
        for (idx, cq_opt) in mig.io_cqs.iter().enumerate() {
            if idx < MAX_IO_QUEUES {
                st.io_cqs[idx] = cq_opt.as_ref().map(|cq| CompQueue {
                    base: cq.base, size: cq.size, head: cq.head,
                    tail: cq.tail, phase: cq.phase, iv: cq.iv,
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

#[cfg(test)]
mod tests {
    use super::*;

    struct NvmeQuiesceTestDevice {
        gate: Arc<QuiesceGate>,
    }

    impl Lifecycle for NvmeQuiesceTestDevice {
        fn type_name(&self) -> &'static str {
            "nvme"
        }

        fn is_quiesced(&self) -> bool {
            self.gate.is_quiesced()
        }
    }

    #[test]
    fn quiesce_query_is_non_blocking_with_active_worker() {
        let device: Arc<dyn Lifecycle> = Arc::new(NvmeQuiesceTestDevice {
            gate: Arc::new(QuiesceGate::new(1)),
        });

        crate::quiesce::testing::assert_query_non_blocking(&device);
        assert!(!device.is_quiesced());
    }

    #[test]
    fn cap_register_value() {
        // Verify CAP register encoding
        let mqes = MAX_QUEUE_SIZE - 1;
        let cap: u64 = (u64::from(mqes) << CAP_MQES_SHIFT)
            | CAP_CQR
            | (1u64 << CAP_TO_SHIFT)
            | CAP_CSS_NVM;
        // MQES should be in bits 15:0
        assert_eq!((cap & CAP_MQES_MASK) as u16, mqes);
        // CQR bit 16
        assert_ne!(cap & CAP_CQR, 0);
        // CSS NVM bit 37
        assert_ne!(cap & CAP_CSS_NVM, 0);
    }

    #[test]
    fn cqe_phase_bit_encoding() {
        // Phase bit is bit 0 of the status word in CQE
        let status: u16 = SC_SUCCESS;
        let phase = true;
        let encoded = (status << 1) | if phase { 1 } else { 0 };
        assert_eq!(encoded & 1, 1); // phase set
        assert_eq!(encoded >> 1, SC_SUCCESS);
    }

    #[test]
    fn doorbell_index_calculation() {
        // SQ tail doorbell for queue 0 is at offset 0x1000
        let offset = REG_DOORBELL_BASE;
        let db_offset = offset - REG_DOORBELL_BASE;
        let db_index = db_offset / 4;
        let qid = db_index / 2;
        let is_sq = (db_index % 2) == 0;
        assert_eq!(qid, 0);
        assert!(is_sq);

        // CQ head doorbell for queue 0 is at offset 0x1004
        let offset = REG_DOORBELL_BASE + 4;
        let db_offset = offset - REG_DOORBELL_BASE;
        let db_index = db_offset / 4;
        let qid = db_index / 2;
        let is_sq = (db_index % 2) == 0;
        assert_eq!(qid, 0);
        assert!(!is_sq);

        // SQ tail doorbell for queue 1 is at offset 0x1008
        let offset = REG_DOORBELL_BASE + 8;
        let db_offset = offset - REG_DOORBELL_BASE;
        let db_index = db_offset / 4;
        let qid = db_index / 2;
        let is_sq = (db_index % 2) == 0;
        assert_eq!(qid, 1);
        assert!(is_sq);
    }

    #[test]
    fn prp_single_page() {
        // When total_len fits in one page, only PRP1 is used
        // This is a structural test of the logic (can't test without MemCtx)
        let offset_in_page = 0x100usize;
        let _prp1_addr = 0x1000u64 + offset_in_page as u64;
        let first_len = (PAGE_SIZE - offset_in_page).min(512);
        assert_eq!(first_len, 512); // 512 < 3840
    }

    #[test]
    fn status_code_encoding() {
        // DNR flag should be bit 14
        assert_eq!(SC_DNR, 1 << 14);
        // Invalid opcode with DNR
        let status = SC_INVALID_OPCODE | SC_DNR;
        assert_ne!(status & SC_DNR, 0);
        assert_eq!(status & !SC_DNR, SC_INVALID_OPCODE);
    }

    #[test]
    fn sector_overflow_checked_mul() {
        // Verify that checked_mul catches overflow in sector arithmetic.
        // slba near u64::MAX with block_size=512 would overflow.
        let slba: u64 = u64::MAX / 512 + 1;
        let block_size: u32 = 512;
        assert!(slba.checked_mul(u64::from(block_size)).is_none());
    }

    #[test]
    fn sector_bounds_checked_add() {
        // slba + nlb overflow should be caught
        let slba: u64 = u64::MAX;
        let nlb: u16 = 1;
        assert!(slba.checked_add(u64::from(nlb)).is_none());
    }

    #[test]
    fn nlb_overflow_checked_mul() {
        // nlb * block_size overflow should be caught
        let nlb: u16 = u16::MAX;
        let block_size: u32 = u32::MAX;
        assert!((nlb as u64).checked_mul(u64::from(block_size)).is_some());
        // This fits in u64, so the overflow is in byte_offset, not byte_count
        // Verify the realistic case works
        let nlb: u16 = 128;
        let block_size: u32 = 512;
        assert_eq!(
            (nlb as u64).checked_mul(u64::from(block_size)),
            Some(65536)
        );
    }
}
