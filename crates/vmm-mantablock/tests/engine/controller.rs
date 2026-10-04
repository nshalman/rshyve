// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The NVMe controller on the ring, built the way rshyve builds it.
//!
//! `NvmeController::with_backend` takes a VM handle for its MSIs, and no
//! fake one exists outside vmm-storage, so these tests make a real VM.
//! They need /dev/vmm and the privilege to make a VM, as well as the
//! engine. MSI-X stays disabled, so the VM never gets an interrupt.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use vmm_core::common::{RWOp, ReadOp, WriteOp};
use vmm_core::hdl::{CreateOpts, VmmHdl};
use vmm_core::mem::{MemCtx, PhysMap};
use vmm_core::mmio::MmioBus;
use vmm_devices::pci::device::PciDevice;
use vmm_devices::pci::BarN;
use vmm_storage::nvme::bits::*;
use vmm_storage::nvme::{NvmeConfig, NvmeController};

use super::{pattern, Lab, BLOCKS, FILE_VOL, NULL_VOL, PAGE};

const RAM: usize = 1 << 20;
const ASQ: u64 = 0x1_0000;
const ACQ: u64 = 0x1_1000;
const SQ1: u64 = 0x1_2000;
const CQ1: u64 = 0x1_3000;
const IDENT: u64 = 0x2_0000;
const DATA: u64 = 0x3_0000;
const QDEPTH: u16 = 16;

/// The build host's zone refuses a second VM while one exists, with
/// EINVAL, so the tests take turns.
static ONE_VM: Mutex<()> = Mutex::new(());

/// A VM that the kernel destroys when its handle closes.
fn vm() -> Arc<VmmHdl> {
    let name = format!("mantablock-test-{}", std::process::id());
    let hdl = VmmHdl::create(&name, &CreateOpts::default()).unwrap();
    hdl.set_autodestruct(true).unwrap();
    Arc::new(hdl)
}

fn controller(
    lab: &Lab,
    vol: &str,
    allow_volatile: bool,
    config: Option<NvmeConfig>,
) -> anyhow::Result<Driver> {
    let turn = ONE_VM.lock().unwrap_or_else(|e| e.into_inner());
    let mut cfg = lab.config(vol);
    cfg.allow_volatile = allow_volatile;
    let conn = vmm_mantablock::connect(cfg)?;
    let config = config.unwrap_or_else(|| conn.nvme_config());
    let physmap = Arc::new(PhysMap::new_anon(0, RAM)?);
    let ctrl = NvmeController::with_backend(
        conn.into_factory(),
        config,
        false,
        1,
        Arc::clone(&physmap),
        vm(),
        Arc::new(MmioBus::new()),
        slog::Logger::root(slog::Discard, slog::o!()),
    )?;
    Ok(Driver {
        ctrl,
        mem: MemCtx::new(physmap),
        admin: Queue::new(0, ASQ, ACQ),
        io: Queue::new(1, SQ1, CQ1),
        cid: 0,
        _turn: turn,
    })
}

struct Queue {
    qid: u16,
    sq: u64,
    cq: u64,
    tail: u16,
    head: u16,
    phase: bool,
}

impl Queue {
    fn new(qid: u16, sq: u64, cq: u64) -> Self {
        Self {
            qid,
            sq,
            cq,
            tail: 0,
            head: 0,
            phase: true,
        }
    }
}

/// Just enough of a guest NVMe driver to reach the ring.
struct Driver {
    ctrl: Arc<NvmeController>,
    mem: MemCtx,
    admin: Queue,
    io: Queue,
    cid: u16,
    /// Last, so the VM is gone before the next test makes one.
    _turn: MutexGuard<'static, ()>,
}

impl Driver {
    fn reg(&self, offset: usize, val: u64, len: usize) {
        let wo = WriteOp::from_buf(&val.to_le_bytes()[..len]);
        self.ctrl.bar_rw(BarN::BAR0, offset, RWOp::Write(&wo));
    }

    fn enable(&mut self) {
        let q = u64::from(QDEPTH - 1);
        self.reg(REG_AQA, q | (q << 16), 4);
        self.reg(REG_ASQ, ASQ, 8);
        self.reg(REG_ACQ, ACQ, 8);
        let cc = CC_EN
            | (SQE_SIZE_LOG2 << CC_IOSQES_SHIFT)
            | (CQE_SIZE_LOG2 << CC_IOCQES_SHIFT);
        self.reg(REG_CC, u64::from(cc), 4);
        let mut ro = ReadOp::new(4);
        self.ctrl.bar_rw(BarN::BAR0, REG_CSTS, RWOp::Read(&mut ro));
        assert_eq!(ro.buf()[0] & CSTS_RDY as u8, 1, "CSTS.RDY");
    }

    /// Submits one command and waits for it. Returns status and DW0.
    fn run(&mut self, io: bool, sqe: [u8; SQE_SIZE]) -> (u16, u32) {
        self.cid += 1;
        let cid = self.cid;
        let q = if io { &mut self.io } else { &mut self.admin };
        let mut sqe = sqe;
        sqe[SQE_CID..SQE_CID + 2].copy_from_slice(&cid.to_le_bytes());
        let slot = q.sq + u64::from(q.tail) * SQE_SIZE as u64;
        self.mem.write(slot, &sqe).unwrap();
        q.tail = (q.tail + 1) % QDEPTH;
        let (qid, tail) = (q.qid, q.tail);
        self.reg(REG_DOORBELL_BASE + usize::from(qid) * 8, tail.into(), 4);

        let q = if io { &mut self.io } else { &mut self.admin };
        let slot = q.cq + u64::from(q.head) * CQE_SIZE as u64;
        let until = Instant::now() + Duration::from_secs(10);
        let mut cqe = [0u8; CQE_SIZE];
        loop {
            self.mem.read(slot, &mut cqe).unwrap();
            if (cqe[CQE_STATUS] & 1 != 0) == q.phase {
                break;
            }
            assert!(Instant::now() < until, "no completion");
            std::thread::sleep(Duration::from_millis(1));
        }
        q.head = (q.head + 1) % QDEPTH;
        if q.head == 0 {
            q.phase = !q.phase;
        }
        let head = q.head;
        self.reg(REG_DOORBELL_BASE + usize::from(qid) * 8 + 4, head.into(), 4);
        assert_eq!(u16::from_le_bytes([cqe[CQE_CID], cqe[CQE_CID + 1]]), cid);
        let status = u16::from_le_bytes([cqe[CQE_STATUS], cqe[CQE_STATUS + 1]]);
        let dw0 =
            u32::from_le_bytes(cqe[CQE_DW0..CQE_DW0 + 4].try_into().unwrap());
        (status >> 1, dw0)
    }

    fn identify(&mut self, cns: u8) -> Vec<u8> {
        let sqe = sqe(ADMIN_OPC_IDENTIFY, 1, IDENT, 0, [cns.into(), 0, 0]);
        assert_eq!(self.run(false, sqe).0, SC_SUCCESS, "identify");
        let mut page = vec![0; PAGE];
        self.mem.read(IDENT, &mut page).unwrap();
        page
    }

    fn io_queues(&mut self) {
        let size = u32::from(QDEPTH - 1) << 16 | 1;
        let cq = sqe(ADMIN_OPC_CREATE_IO_CQ, 0, CQ1, 0, [size, CQ_PC, 0]);
        assert_eq!(self.run(false, cq).0, SC_SUCCESS, "create CQ");
        let sq = sqe(
            ADMIN_OPC_CREATE_IO_SQ,
            0,
            SQ1,
            0,
            [size, 1 << 16 | CQ_PC, 0],
        );
        assert_eq!(self.run(false, sq).0, SC_SUCCESS, "create SQ");
    }

    /// Two 4 KiB blocks at `lba`, from or to `DATA`.
    fn rw(&mut self, opcode: u8, lba: u64) -> u16 {
        let prp2 = DATA + PAGE as u64;
        let cdw = [lba as u32, (lba >> 32) as u32, 1];
        self.run(true, sqe(opcode, 1, DATA, prp2, cdw)).0
    }
}

fn sqe(opc: u8, nsid: u32, prp1: u64, prp2: u64, cdw: [u32; 3]) -> [u8; 64] {
    let mut b = [0u8; SQE_SIZE];
    b[SQE_OPC] = opc;
    b[SQE_NSID..SQE_NSID + 4].copy_from_slice(&nsid.to_le_bytes());
    b[SQE_PRP1..SQE_PRP1 + 8].copy_from_slice(&prp1.to_le_bytes());
    b[SQE_PRP2..SQE_PRP2 + 8].copy_from_slice(&prp2.to_le_bytes());
    for (i, w) in cdw.iter().enumerate() {
        let at = SQE_CDW10 + 4 * i;
        b[at..at + 4].copy_from_slice(&w.to_le_bytes());
    }
    b
}

#[test]
#[ignore = "needs MANTABLOCK_ENGINE and the privilege to make a VM"]
fn a_durable_volume_is_a_4k_namespace_without_a_write_cache() {
    let lab = Lab::start();
    let mut d = controller(&lab, FILE_VOL, false, None).unwrap();
    d.enable();

    let ns = d.identify(IDENTIFY_CNS_NAMESPACE);
    let nsze = u64::from_le_bytes(ns[0..8].try_into().unwrap());
    let lbaf0 = u32::from_le_bytes(ns[128..132].try_into().unwrap());
    assert_eq!(nsze, BLOCKS);
    assert_eq!((lbaf0 >> 16) & 0xFF, 12, "LBADS");
    assert_eq!(d.identify(IDENTIFY_CNS_CONTROLLER)[525], 0, "VWC");

    d.io_queues();
    let data = pattern(2, 0x5a);
    d.mem.write(DATA, &data).unwrap();
    assert_eq!(d.rw(NVM_OPC_WRITE, 7), SC_SUCCESS);
    assert_eq!(lab.on_disk(7, 2), data);

    d.mem.write(DATA, &vec![0; 2 * PAGE]).unwrap();
    assert_eq!(d.rw(NVM_OPC_READ, 7), SC_SUCCESS);
    let mut back = vec![0; 2 * PAGE];
    d.mem.read(DATA, &mut back).unwrap();
    assert_eq!(back, data);
}

#[test]
#[ignore = "needs MANTABLOCK_ENGINE and the privilege to make a VM"]
fn a_volatile_volume_needs_a_write_cache() {
    let lab = Lab::start();
    let cfg = NvmeConfig {
        volatile_write_cache: false,
        ..NvmeConfig::default()
    };
    let refused = controller(&lab, NULL_VOL, true, Some(cfg));
    assert!(refused.err().unwrap().to_string().contains("durable"));

    let mut d = controller(&lab, NULL_VOL, true, None).unwrap();
    d.enable();
    assert_eq!(d.identify(IDENTIFY_CNS_CONTROLLER)[525], 1, "VWC");
}
