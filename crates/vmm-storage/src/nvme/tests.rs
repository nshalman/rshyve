// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use vmm_core::common::{RWOp, ReadOp, WriteOp, PAGE_SIZE};
use vmm_core::mem::{MemCtx, PhysMap};
use vmm_core::mmio::MmioBus;
use vmm_devices::pci::device::PciDevice;
use vmm_devices::pci::msix::MsiSink;
use vmm_devices::pci::BarN;
use vmm_devices::{Lifecycle, QuiesceGate};

use super::bits::*;
use super::queues::NvmeState;
use super::{NvmeController, NvmeParts};

/// Guest RAM the harness maps: enough for queues and a 2 MiB transfer.
const GUEST_RAM: usize = 8 << 20;
const BACKING_BLOCKS: u64 = 4096;
const ASQ: u64 = 0x10000;
const ACQ: u64 = 0x11000;
const ADMIN_QDEPTH: u16 = 16;
const WAIT: Duration = Duration::from_secs(5);

struct RecordingSink(Mutex<Vec<(u64, u64)>>);

impl MsiSink for RecordingSink {
    fn send(&self, addr: u64, data: u64) {
        self.0.lock().expect("sink lock").push((addr, data));
    }
}

/// One controller over anonymous guest RAM and a patterned temp file.
///
/// Byte `i` of the backing file is `(i / 512) as u8 ^ (i as u8)`, so a
/// read that lands in the wrong page is visible.
struct Harness {
    ctrl: Arc<NvmeController>,
    physmap: Arc<PhysMap>,
    sink: Arc<RecordingSink>,
    admin_tail: u16,
    admin_cq_head: usize,
    admin_phase: bool,
    next_cid: u16,
}

fn backing_byte(i: u64) -> u8 {
    ((i / 512) as u8) ^ (i as u8)
}

fn backing_file() -> std::fs::File {
    let mut file = tempfile::tempfile().expect("temp backing file");
    let bytes: Vec<u8> = (0..BACKING_BLOCKS * 512).map(backing_byte).collect();
    file.write_all(&bytes).expect("fill backing file");
    file
}

/// A backend whose transfers the test controls.
///
/// A held transfer stands in for a slow disk: the test can stop every
/// worker inside the backend and then do to the controller whatever a
/// guest could do while its I/O is outstanding.
struct TestBackend {
    file: Arc<std::fs::File>,
    delay_us: AtomicU64,
    state: Mutex<BackendState>,
    cv: Condvar,
}

#[derive(Default)]
struct BackendState {
    held: bool,
    /// Transfers that have entered the backend, ever.
    started: usize,
    /// Transfers inside the backend now, held ones included.
    in_flight: usize,
}

impl TestBackend {
    fn new(file: Arc<std::fs::File>) -> Arc<Self> {
        Arc::new(Self {
            file,
            delay_us: AtomicU64::new(0),
            state: Mutex::new(BackendState::default()),
            cv: Condvar::new(),
        })
    }

    fn set_delay(&self, delay: Duration) {
        self.delay_us
            .store(delay.as_micros() as u64, Ordering::Relaxed);
    }

    /// Stop every transfer inside the backend until [`Self::release`].
    fn hold(&self) {
        self.state.lock().expect("backend lock").held = true;
    }

    fn release(&self) {
        let mut st = self.state.lock().expect("backend lock");
        st.held = false;
        self.cv.notify_all();
    }

    /// Transfers that have entered the backend, ever.
    fn started(&self) -> usize {
        self.state.lock().expect("backend lock").started
    }

    fn in_flight(&self) -> usize {
        self.state.lock().expect("backend lock").in_flight
    }

    /// Block until `n` transfers are inside the backend.
    fn wait_in_flight(&self, n: usize) {
        let mut st = self.state.lock().expect("backend lock");
        let start = Instant::now();
        while st.in_flight < n {
            let (next, timeout) = self
                .cv
                .wait_timeout(st, Duration::from_millis(50))
                .expect("backend lock");
            st = next;
            assert!(
                !timeout.timed_out() || start.elapsed() < WAIT,
                "only {} of {n} transfers started",
                st.in_flight
            );
        }
    }

    fn enter(&self) {
        let mut st = self.state.lock().expect("backend lock");
        st.started += 1;
        st.in_flight += 1;
        self.cv.notify_all();
        while st.held {
            st = self.cv.wait(st).expect("backend lock");
        }
        drop(st);
        let delay = self.delay_us.load(Ordering::Relaxed);
        if delay != 0 {
            std::thread::sleep(Duration::from_micros(delay));
        }
    }

    fn leave(&self) {
        let mut st = self.state.lock().expect("backend lock");
        st.in_flight -= 1;
        self.cv.notify_all();
    }
}

impl super::io::IoBackend for TestBackend {
    fn read_at(
        &self,
        iov: &vmm_core::mem::GuestIoVec,
        offset: u64,
    ) -> std::io::Result<usize> {
        self.enter();
        let done = iov.read_from(&self.file, offset);
        self.leave();
        done
    }

    fn write_at(
        &self,
        iov: &vmm_core::mem::GuestIoVec,
        offset: u64,
    ) -> std::io::Result<usize> {
        self.enter();
        let done = iov.write_to(&self.file, offset);
        self.leave();
        done
    }

    fn sync(&self) -> std::io::Result<()> {
        self.enter();
        let done = self.file.sync_data();
        self.leave();
        done
    }
}

impl Harness {
    /// A harness whose transfers the returned backend controls.
    fn held() -> (Self, Arc<TestBackend>) {
        let file = Arc::new(backing_file());
        let backend = TestBackend::new(Arc::clone(&file));
        let h = Self::with_backend(
            file,
            Arc::clone(&backend) as Arc<dyn super::io::IoBackend>,
        );
        (h, backend)
    }

    fn new() -> Self {
        let file = Arc::new(backing_file());
        Self::with_backend(Arc::clone(&file), file)
    }

    fn with_backend(
        file: Arc<std::fs::File>,
        backend: Arc<dyn super::io::IoBackend>,
    ) -> Self {
        let physmap =
            Arc::new(PhysMap::new_anon(0, GUEST_RAM).expect("anon guest RAM"));
        let sink = Arc::new(RecordingSink(Mutex::new(Vec::new())));
        let ctrl = NvmeController::build(NvmeParts {
            file,
            backend,
            read_only: false,
            instance_id: 1,
            physmap: Arc::clone(&physmap),
            msi: Arc::clone(&sink) as Arc<dyn MsiSink>,
            bus_mmio: Arc::new(MmioBus::new()),
            log: slog::Logger::root(slog::Discard, slog::o!()),
        });
        ctrl.msix.set_enabled(true);
        for v in 0..NVME_MSIX_COUNT {
            ctrl.msix.write_entry(v, 0xFEE0_0000, u64::from(v));
        }
        Self {
            ctrl,
            physmap,
            sink,
            admin_tail: 0,
            admin_cq_head: 0,
            admin_phase: true,
            next_cid: 1,
        }
    }

    fn mem(&self) -> MemCtx {
        MemCtx::new(Arc::clone(&self.physmap))
    }

    fn fill(&self, gpa: u64, len: usize, byte: u8) {
        self.mem()
            .write(gpa, &vec![byte; len])
            .expect("fill guest RAM");
    }

    fn read_guest(&self, gpa: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        self.mem().read(gpa, &mut buf).expect("read guest RAM");
        buf
    }

    fn reg_write32(&self, offset: usize, val: u32) {
        let wo = WriteOp::from_buf(&val.to_le_bytes());
        self.ctrl.bar_rw(BarN::BAR0, offset, RWOp::Write(&wo));
    }

    fn reg_write64(&self, offset: usize, val: u64) {
        let wo = WriteOp::from_buf(&val.to_le_bytes());
        self.ctrl.bar_rw(BarN::BAR0, offset, RWOp::Write(&wo));
    }

    fn reg_read32(&self, offset: usize) -> u32 {
        let mut ro = ReadOp::new(4);
        self.ctrl.bar_rw(BarN::BAR0, offset, RWOp::Read(&mut ro));
        u32::from_le_bytes(ro.buf().try_into().expect("four bytes"))
    }

    fn sq_doorbell(&self, qid: u16, tail: u16) {
        self.reg_write32(
            REG_DOORBELL_BASE + usize::from(qid) * 8,
            u32::from(tail),
        );
    }

    fn cq_doorbell(&self, qid: u16, head: u16) {
        self.reg_write32(
            REG_DOORBELL_BASE + usize::from(qid) * 8 + 4,
            u32::from(head),
        );
    }

    /// Program the admin queues and set CC.EN with the entry sizes a
    /// real driver writes.
    ///
    /// The controller builds fresh admin queues, so the harness starts
    /// its own head, tail and phase over too.
    fn enable(&mut self) {
        self.admin_tail = 0;
        self.admin_cq_head = 0;
        self.admin_phase = true;
        self.reg_write32(
            REG_AQA,
            u32::from(ADMIN_QDEPTH - 1) | (u32::from(ADMIN_QDEPTH - 1) << 16),
        );
        self.reg_write64(REG_ASQ, ASQ);
        self.reg_write64(REG_ACQ, ACQ);
        self.reg_write32(
            REG_CC,
            CC_EN
                | (SQE_SIZE_LOG2 << CC_IOSQES_SHIFT)
                | (CQE_SIZE_LOG2 << CC_IOCQES_SHIFT),
        );
    }

    fn csts(&self) -> u32 {
        self.reg_read32(REG_CSTS)
    }

    /// Submit one admin command and return its completion.
    fn admin(&mut self, sqe: Sqe) -> Cqe {
        let cid = self.next_cid;
        self.next_cid += 1;
        let entry = ASQ + u64::from(self.admin_tail) * SQE_SIZE as u64;
        self.mem()
            .write(entry, &sqe.encode(cid))
            .expect("write SQE");
        self.admin_tail = (self.admin_tail + 1) % ADMIN_QDEPTH;
        self.sq_doorbell(ADMIN_QUEUE_ID, self.admin_tail);

        let slot = ACQ + self.admin_cq_head as u64 * CQE_SIZE as u64;
        let cqe = Cqe::decode(&self.read_guest(slot, CQE_SIZE));
        assert_eq!(cqe.phase, self.admin_phase, "admin CQE phase");
        assert_eq!(cqe.cid, cid, "admin CQE cid");
        self.admin_cq_head += 1;
        if self.admin_cq_head == usize::from(ADMIN_QDEPTH) {
            self.admin_cq_head = 0;
            self.admin_phase = !self.admin_phase;
        }
        self.cq_doorbell(ADMIN_QUEUE_ID, self.admin_cq_head as u16);
        cqe
    }

    /// Create I/O queue pair `qid` of `depth` entries with the SQ at
    /// `sq_base` and the CQ at `cq_base`, MSI-X vector `qid`.
    fn create_io_queues(
        &mut self,
        qid: u16,
        depth: u16,
        sq_base: u64,
        cq_base: u64,
    ) {
        let cqe = self.admin(Sqe {
            opcode: ADMIN_OPC_CREATE_IO_CQ,
            prp1: cq_base,
            cdw10: u32::from(qid) | (u32::from(depth - 1) << 16),
            cdw11: (u32::from(qid) << 16) | CQ_IEN | CQ_PC,
            ..Sqe::default()
        });
        assert_eq!(cqe.status(), Status::SUCCESS, "create CQ");
        let cqe = self.admin(Sqe {
            opcode: ADMIN_OPC_CREATE_IO_SQ,
            prp1: sq_base,
            cdw10: u32::from(qid) | (u32::from(depth - 1) << 16),
            cdw11: (u32::from(qid) << 16) | CQ_PC,
            ..Sqe::default()
        });
        assert_eq!(cqe.status(), Status::SUCCESS, "create SQ");
    }

    /// Poll a CQ slot until its phase bit reads `phase`.
    fn wait_cqe(&self, cq_base: u64, index: u16, phase: bool) -> Cqe {
        let slot = cq_base + u64::from(index) * CQE_SIZE as u64;
        let start = Instant::now();
        loop {
            let cqe = Cqe::decode(&self.read_guest(slot, CQE_SIZE));
            if cqe.phase == phase {
                return cqe;
            }
            assert!(start.elapsed() < WAIT, "no completion in {WAIT:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn msi_count(&self) -> usize {
        self.sink.0.lock().expect("sink lock").len()
    }
}

#[derive(Default, Clone, Copy)]
struct Sqe {
    opcode: u8,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
}

impl Sqe {
    fn encode(&self, cid: u16) -> [u8; SQE_SIZE] {
        let mut b = [0u8; SQE_SIZE];
        b[SQE_OPC] = self.opcode;
        b[SQE_CID..SQE_CID + 2].copy_from_slice(&cid.to_le_bytes());
        b[SQE_NSID..SQE_NSID + 4].copy_from_slice(&self.nsid.to_le_bytes());
        b[SQE_PRP1..SQE_PRP1 + 8].copy_from_slice(&self.prp1.to_le_bytes());
        b[SQE_PRP2..SQE_PRP2 + 8].copy_from_slice(&self.prp2.to_le_bytes());
        b[SQE_CDW10..SQE_CDW10 + 4].copy_from_slice(&self.cdw10.to_le_bytes());
        b[SQE_CDW11..SQE_CDW11 + 4].copy_from_slice(&self.cdw11.to_le_bytes());
        b[SQE_CDW12..SQE_CDW12 + 4].copy_from_slice(&self.cdw12.to_le_bytes());
        b
    }

    fn read(slba: u64, nlb: u16, prp1: u64, prp2: u64) -> Self {
        Self {
            opcode: NVM_OPC_READ,
            nsid: 1,
            prp1,
            prp2,
            cdw10: slba as u32,
            cdw11: (slba >> 32) as u32,
            cdw12: u32::from(nlb - 1),
        }
    }

    fn write(slba: u64, nlb: u16, prp1: u64, prp2: u64) -> Self {
        Self {
            opcode: NVM_OPC_WRITE,
            ..Self::read(slba, nlb, prp1, prp2)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Status {
    sct: u8,
    sc: u8,
    dnr: bool,
}

impl Status {
    const SUCCESS: Self = Self {
        sct: 0,
        sc: 0,
        dnr: false,
    };

    /// The status a `SC_*` constant encodes, as the guest decodes it.
    fn of(code: u16) -> Self {
        Self {
            sct: ((code >> 8) & 0x7) as u8,
            sc: (code & 0xFF) as u8,
            dnr: code & SC_DNR != 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Cqe {
    dw0: u32,
    sq_head: u16,
    sqid: u16,
    cid: u16,
    raw_status: u16,
    phase: bool,
}

impl Cqe {
    fn decode(b: &[u8]) -> Self {
        let raw = u16::from_le_bytes([b[CQE_STATUS], b[CQE_STATUS + 1]]);
        Self {
            dw0: u32::from_le_bytes(
                b[CQE_DW0..CQE_DW0 + 4].try_into().expect("dw0"),
            ),
            sq_head: u16::from_le_bytes([b[CQE_SQHD], b[CQE_SQHD + 1]]),
            sqid: u16::from_le_bytes([b[CQE_SQID], b[CQE_SQID + 1]]),
            cid: u16::from_le_bytes([b[CQE_CID], b[CQE_CID + 1]]),
            raw_status: raw,
            phase: raw & 1 != 0,
        }
    }

    fn status(&self) -> Status {
        Status {
            sct: ((self.raw_status >> 9) & 0x7) as u8,
            sc: ((self.raw_status >> 1) & 0xFF) as u8,
            dnr: self.raw_status & (1 << 15) != 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Register and admin path
// ---------------------------------------------------------------------------

#[test]
fn enable_sets_rdy_and_identify_lands_through_prp1_and_prp2() {
    let mut h = Harness::new();
    h.enable();
    assert_eq!(h.csts() & CSTS_RDY, CSTS_RDY);

    // PRP1 starts mid-page, so the 4 KiB structure straddles PRP2.
    let prp1 = 0x20800;
    let prp2 = 0x22000;
    h.fill(0x20000, 3 * PAGE_SIZE, 0xAA);
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_IDENTIFY,
        prp1,
        prp2,
        cdw10: u32::from(IDENTIFY_CNS_CONTROLLER),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(cqe.sqid, ADMIN_QUEUE_ID);
    assert_eq!(cqe.sq_head, 1);
    assert_eq!(cqe.dw0, 0);
    assert_eq!(h.msi_count(), 1, "one MSI for the admin CQ");

    let first = h.read_guest(prp1, 0x800);
    assert_eq!(&first[4..24], b"TRITON-NVME-0001    ");
    assert_eq!(first[77], MDTS_LOG2_PAGES);
    assert_eq!(
        first[512],
        SQE_SIZE_LOG2 as u8 | ((SQE_SIZE_LOG2 as u8) << 4)
    );
    let second = h.read_guest(prp2, 0x800);
    assert_eq!(first[525], 1, "VWC");
    assert!(
        second.iter().all(|&b| b == 0),
        "upper half of Identify is zero"
    );
    // Nothing past the end of the PRP1 page and nothing past the data.
    assert!(h.read_guest(0x21000, PAGE_SIZE).iter().all(|&b| b == 0xAA));
    assert!(h.read_guest(prp2 + 0x800, 0x800).iter().all(|&b| b == 0xAA));
}

#[test]
fn admin_cq_wraps_and_flips_phase() {
    let mut h = Harness::new();
    h.enable();
    for _ in 0..ADMIN_QDEPTH + 2 {
        let cqe = h.admin(Sqe {
            opcode: ADMIN_OPC_GET_FEATURES,
            cdw10: u32::from(FEAT_NUM_QUEUES),
            ..Sqe::default()
        });
        assert_eq!(cqe.status(), Status::SUCCESS);
    }
    // The harness asserted the phase on every entry; past the wrap the
    // controller must be writing phase 0.
    assert!(!h.admin_phase);
    assert_eq!(h.admin_cq_head, 2);
}

#[test]
fn admin_doorbell_beyond_queue_size_is_ignored() {
    let mut h = Harness::new();
    h.enable();
    let entry = ASQ;
    let sqe = Sqe {
        opcode: ADMIN_OPC_GET_FEATURES,
        cdw10: u32::from(FEAT_NUM_QUEUES),
        ..Sqe::default()
    };
    h.mem().write(entry, &sqe.encode(7)).expect("write SQE");
    h.sq_doorbell(ADMIN_QUEUE_ID, ADMIN_QDEPTH);
    assert_eq!(h.read_guest(ACQ, CQE_SIZE), vec![0u8; CQE_SIZE]);
    assert_eq!(h.msi_count(), 0);
}

#[test]
fn get_log_page_stops_at_the_end_of_the_prp1_page() {
    let mut h = Harness::new();
    h.enable();

    // 256 bytes left in the PRP1 page, so half the log goes to PRP2.
    let prp1 = 0x20F00;
    let prp2 = 0x22000;
    h.fill(0x20000, 4 * PAGE_SIZE, 0xAA);
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_GET_LOG_PAGE,
        prp1,
        prp2,
        cdw10: u32::from(LID_SMART) | (((LOG_SMART_SIZE as u32 / 4) - 1) << 16),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);

    let expect = build_log_page(LID_SMART).expect("SMART log");
    assert_eq!(h.read_guest(prp1, 256), expect[..256]);
    assert_eq!(h.read_guest(prp2, 256), expect[256..]);
    // The page after PRP1's is untouched.
    assert!(h.read_guest(0x21000, PAGE_SIZE).iter().all(|&b| b == 0xAA));
    // And so is the rest of the PRP2 page.
    assert!(h.read_guest(prp2 + 256, 256).iter().all(|&b| b == 0xAA));
}

#[test]
fn get_log_page_refuses_a_log_the_controller_does_not_have() {
    let mut h = Harness::new();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_GET_LOG_PAGE,
        prp1: 0x20000,
        cdw10: 0x7F | (0x7F << 16),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_INVALID_LOG_PAGE | SC_DNR));
    assert!(h.read_guest(0x20000, 512).iter().all(|&b| b == 0));
}

// ---------------------------------------------------------------------------
// I/O path
// ---------------------------------------------------------------------------

const SQ1: u64 = 0x30000;
const CQ1: u64 = 0x40000;
const IO_QDEPTH: u16 = 64;

/// Submit `sqe` on I/O queue 1 at `tail` and wait for its completion.
fn io_submit(h: &mut Harness, tail: &mut u16, sqe: Sqe, cid: u16) {
    let entry = SQ1 + u64::from(*tail) * SQE_SIZE as u64;
    h.mem().write(entry, &sqe.encode(cid)).expect("write SQE");
    *tail = (*tail + 1) % IO_QDEPTH;
    h.sq_doorbell(1, *tail);
}

#[test]
fn io_read_through_prp1_and_prp2_returns_the_backing_bytes() {
    let mut h = Harness::new();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let prp1 = 0x50800;
    let prp2 = 0x52000;
    h.fill(0x50000, 3 * PAGE_SIZE, 0xAA);
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(8, 8, prp1, prp2), 0x77);
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(cqe.cid, 0x77);
    assert_eq!(cqe.sqid, 1);
    assert_eq!(cqe.sq_head, 1);

    let expect: Vec<u8> = (8 * 512..16 * 512).map(backing_byte).collect();
    assert_eq!(h.read_guest(prp1, 0x800), expect[..0x800]);
    assert_eq!(h.read_guest(prp2, 0x800), expect[0x800..]);
    assert!(h.read_guest(0x51000, PAGE_SIZE).iter().all(|&b| b == 0xAA));
    assert_eq!(h.msi_count(), 3, "two admin completions and one I/O");
}

#[test]
fn io_write_reaches_the_backing_file() {
    let file = Arc::new(backing_file());
    let mut h = Harness::with_backend(
        Arc::clone(&file),
        Arc::clone(&file) as Arc<dyn super::io::IoBackend>,
    );
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    h.fill(0x50000, 512, 0x5A);
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::write(100, 1, 0x50000, 0), 1);
    assert_eq!(h.wait_cqe(CQ1, 0, true).status(), Status::SUCCESS);

    use std::os::unix::fs::FileExt;
    let mut buf = [0u8; 512];
    file.read_exact_at(&mut buf, 100 * 512).expect("read back");
    assert!(buf.iter().all(|&b| b == 0x5A));
}

/// Write a PRP list entry at `gpa`.
fn put_prp(h: &Harness, gpa: u64, entry: u64) {
    h.mem().write(gpa, &entry.to_le_bytes()).expect("PRP entry");
}

#[test]
fn io_read_follows_a_prp_list_that_chains_at_the_page_end() {
    let mut h = Harness::new();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    // The list starts one entry short of its page end, so the walker
    // must treat that last entry as a chain to the second list.
    let first_page = 0x60000;
    let list_a = 0x61FF8;
    let list_b = 0x62000;
    let (page_b, page_c) = (0x63000, 0x64000);
    h.fill(0x60000, 5 * PAGE_SIZE, 0xAA);
    put_prp(&h, list_a, list_b);
    put_prp(&h, list_b, page_b);
    put_prp(&h, list_b + 8, page_c);

    let mut tail = 0;
    io_submit(
        &mut h,
        &mut tail,
        Sqe::read(0, 24, first_page, list_a),
        0x33,
    );
    assert_eq!(h.wait_cqe(CQ1, 0, true).status(), Status::SUCCESS);

    let expect: Vec<u8> = (0..24 * 512).map(backing_byte).collect();
    assert_eq!(h.read_guest(first_page, PAGE_SIZE), expect[..PAGE_SIZE]);
    assert_eq!(
        h.read_guest(page_b, PAGE_SIZE),
        expect[PAGE_SIZE..2 * PAGE_SIZE]
    );
    assert_eq!(h.read_guest(page_c, PAGE_SIZE), expect[2 * PAGE_SIZE..]);
    // The page holding the first list is untouched past its entry.
    assert!(h.read_guest(0x61000, 0xFF8).iter().all(|&b| b == 0xAA));
}

#[test]
fn prp2_with_a_page_offset_is_refused() {
    let mut h = Harness::new();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_IDENTIFY,
        prp1: 0x20800,
        prp2: 0x22008,
        cdw10: u32::from(IDENTIFY_CNS_CONTROLLER),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_DATA_XFER_ERROR));
    assert!(h.read_guest(0x22000, PAGE_SIZE).iter().all(|&b| b == 0));
}

#[test]
fn prp_list_entry_with_a_page_offset_is_refused() {
    let mut h = Harness::new();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let list = 0x61000;
    h.fill(0x60000, 5 * PAGE_SIZE, 0xAA);
    put_prp(&h, list, 0x63040);
    put_prp(&h, list + 8, 0x64000);

    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 24, 0x60000, list), 0x34);
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.status(), Status::of(SC_DATA_XFER_ERROR));
    // Nothing was transferred: the walk fails before any lookup.
    assert!(h.read_guest(0x60000, PAGE_SIZE).iter().all(|&b| b == 0xAA));
}

#[test]
fn io_read_past_the_namespace_is_refused() {
    let mut h = Harness::new();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut tail = 0;
    io_submit(
        &mut h,
        &mut tail,
        Sqe::read(BACKING_BLOCKS - 1, 2, 0x50000, 0),
        1,
    );
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.status(), Status::of(SC_INVALID_FIELD | SC_DNR));
}

#[test]
fn command_specific_status_reaches_the_guest_with_sct_one() {
    let mut h = Harness::new();
    h.enable();
    // An SQ whose CQ does not exist.
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: 3 << 16,
        ..Sqe::default()
    });
    assert_eq!(
        cqe.status(),
        Status {
            sct: 1,
            sc: 0x00,
            dnr: true
        }
    );
    // A queue id past the allocation.
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10: 16 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: 1 << 16,
        ..Sqe::default()
    });
    assert_eq!(
        cqe.status(),
        Status {
            sct: 1,
            sc: 0x01,
            dnr: true
        }
    );
    // A queue of one entry.
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10: 1,
        cdw11: 1 << 16,
        ..Sqe::default()
    });
    assert_eq!(
        cqe.status(),
        Status {
            sct: 1,
            sc: 0x02,
            dnr: true
        }
    );
}

// ---------------------------------------------------------------------------
// Pause and shutdown
// ---------------------------------------------------------------------------

/// Keep ringing the I/O SQ 1 doorbell until `stop` is set.
///
/// The channel never empties, which is the load an operator pause has
/// to survive.
fn steady_submitter(
    ctrl: Arc<NvmeController>,
    physmap: Arc<PhysMap>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mem = MemCtx::new(physmap);
        let mut tail = 0u16;
        let mut cid = 1u16;
        while !stop.load(Ordering::Relaxed) {
            let sqe = Sqe::read(0, 1, 0x70000, 0);
            mem.write(
                SQ1 + u64::from(tail) * SQE_SIZE as u64,
                &sqe.encode(cid),
            )
            .expect("write SQE");
            tail = (tail + 1) % IO_QDEPTH;
            cid = cid.wrapping_add(1);
            let wo = WriteOp::from_buf(&u32::from(tail).to_le_bytes());
            ctrl.bar_rw(BarN::BAR0, REG_DOORBELL_BASE + 8, RWOp::Write(&wo));
            std::thread::sleep(Duration::from_micros(200));
        }
    })
}

#[test]
fn pause_quiesces_the_workers_under_steady_guest_io() {
    let (mut h, backend) = Harness::held();
    // Slower than the submitter, so the channel keeps growing.
    backend.set_delay(Duration::from_millis(5));
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let stop = Arc::new(AtomicBool::new(false));
    let submitter = steady_submitter(
        Arc::clone(&h.ctrl),
        Arc::clone(&h.physmap),
        Arc::clone(&stop),
    );
    backend.wait_in_flight(1);

    h.ctrl.pause();
    let quiesced = h.ctrl.gate.wait_quiesced(Duration::from_secs(2));

    stop.store(true, Ordering::Relaxed);
    submitter.join().expect("submitter");
    backend.set_delay(Duration::ZERO);
    h.ctrl.resume();
    assert!(quiesced, "pause did not quiesce the workers under load");
}

/// Release the backend once a reset reaches its drain.
///
/// The reset blocks on the vCPU thread that wrote the register, so the
/// transfer it is waiting for has to be let go from somewhere else.
/// Returns whether a drain ever started.
fn release_on_drain(
    gate: Arc<QuiesceGate>,
    backend: Arc<TestBackend>,
) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut drained = false;
        while start.elapsed() < WAIT {
            if gate.drain_count() > 0 {
                drained = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        backend.release();
        drained
    })
}

#[test]
fn a_reset_waits_for_the_transfer_it_interrupts() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    backend.hold();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(8, 8, 0x50000, 0x52000), 0x77);
    backend.wait_in_flight(1);

    let releaser =
        release_on_drain(Arc::clone(&h.ctrl.gate), Arc::clone(&backend));
    h.reg_write32(REG_CC, 0);
    let quiet = backend.in_flight();
    let drained = releaser.join().expect("releaser");

    assert!(drained, "the reset did not drain the workers");
    assert_eq!(quiet, 0, "CC.EN cleared with a transfer still in flight");
    assert_eq!(h.csts() & CSTS_RDY, 0);
    assert_eq!(backend.started(), 1);
    // The interrupted command never completes: its queues are gone.
    assert_eq!(h.read_guest(CQ1, CQE_SIZE), vec![0u8; CQE_SIZE]);
}

#[test]
fn a_reset_drops_queued_io_instead_of_completing_it_into_new_queues() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    // Park the workers so the command stays in the channel across the
    // reset, the way a busy controller leaves work behind.
    h.ctrl.pause();
    assert!(h.ctrl.gate.wait_quiesced(WAIT), "workers did not park");
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(8, 8, 0x50000, 0x52000), 0x99);

    h.reg_write32(REG_CC, 0);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    h.ctrl.resume();

    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x21);
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.cid, 0x21, "a command from before the reset completed");
    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(backend.started(), 1, "the dropped command still ran");
}

#[test]
fn deleting_and_recreating_an_io_queue_drops_the_old_commands() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    h.ctrl.pause();
    assert!(h.ctrl.gate.wait_quiesced(WAIT), "workers did not park");
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(8, 8, 0x50000, 0x52000), 0x99);

    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_DELETE_IO_SQ,
        cdw10: 1,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS, "delete SQ");
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (1 << 16) | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS, "recreate SQ");
    h.ctrl.resume();

    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x21);
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.cid, 0x21, "a command from the deleted queue completed");
    assert_eq!(backend.started(), 1, "the dropped command still ran");
}

#[test]
fn halt_joins_the_io_workers() {
    let (h, backend) = Harness::held();
    let with_workers = Arc::strong_count(&backend);
    assert!(
        with_workers > super::NUM_WORKERS,
        "the workers should hold the backend"
    );

    h.ctrl.halt();

    assert_eq!(
        Arc::strong_count(&backend),
        with_workers - super::NUM_WORKERS,
        "halt left workers running"
    );
}

#[test]
fn dropping_the_controller_exits_the_io_workers() {
    let (h, backend) = Harness::held();
    let with_workers = Arc::strong_count(&backend);

    drop(h);

    assert_eq!(
        Arc::strong_count(&backend),
        with_workers - super::NUM_WORKERS,
        "the worker pool kept the controller alive"
    );
}

// ---------------------------------------------------------------------------
// Unit-level checks kept from before the harness
// ---------------------------------------------------------------------------

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

    let probe = Arc::clone(&device);
    assert!(!vmm_devices_testsupport::assert_query_non_blocking(
        move || probe.is_quiesced()
    ));
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
fn create_io_cq_with_an_unbacked_interrupt_vector_is_rejected() {
    let mut h = Harness::new();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (u32::from(NVME_MSIX_COUNT) << 16) | CQ_IEN | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_INVALID_INTR_VECTOR | SC_DNR));
}

#[test]
fn a_polled_completion_queue_raises_no_interrupt() {
    let mut h = Harness::new();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (1 << 16) | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (1 << 16) | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);

    let admin_msi = h.msi_count();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x41);
    assert_eq!(h.wait_cqe(CQ1, 0, true).cid, 0x41);
    assert_eq!(h.msi_count(), admin_msi, "IEN was clear");
}

#[test]
fn create_io_queue_past_the_allocation_is_rejected() {
    let mut h = Harness::new();
    h.enable();
    // Ask for four queue pairs, 0-based in CDW11.
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_SET_FEATURES,
        cdw10: u32::from(FEAT_NUM_QUEUES),
        cdw11: 3 | (3 << 16),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);

    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10: 5 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (1 << 16) | CQ_IEN | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_INVALID_QUEUE_ID | SC_DNR));
}

#[test]
fn create_io_sq_naming_the_admin_cq_is_rejected() {
    let mut h = Harness::new();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_CQ_INVALID | SC_DNR));
}

#[test]
fn create_io_queue_larger_than_the_controller_allows_is_rejected() {
    let mut h = Harness::new();
    h.enable();
    // 0-based, so this asks for 65536 entries.
    let cdw10 = 1 | (u32::from(u16::MAX) << 16);
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10,
        cdw11: CQ_IEN | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_INVALID_QUEUE_SIZE | SC_DNR));
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ1,
        cdw10,
        cdw11: 1 << 16 | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_INVALID_QUEUE_SIZE | SC_DNR));
}

#[test]
fn num_queues_max_request_stays_nonzero() {
    let mut state = NvmeState::default();

    let set_result = NvmeController::admin_set_features(
        &mut state,
        u32::from(FEAT_NUM_QUEUES),
        u32::MAX,
    );

    let allocated = (MAX_IO_QUEUES as u32) - 1;
    assert_eq!(set_result, (SC_SUCCESS, allocated | (allocated << 16)));
    assert_eq!(state.num_io_queues, MAX_IO_QUEUES as u16);
    assert_ne!(state.num_io_queues, 0);
    assert_eq!(
        NvmeController::admin_get_features(&state, u32::from(FEAT_NUM_QUEUES),),
        set_result
    );
}
