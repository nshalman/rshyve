// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the in-flight budget.
//!
//! A pause waits for every request the device took, and the guest
//! picks how much it posts. The budget is what keeps that wait short,
//! and the kick is what keeps a ring it left work in from stalling.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::pci::notify::BackendKick;

const LIMIT: u64 = 1 << 20;
const REQ: usize = 256 * 1024;
const RING: u16 = 64;
const BASE: u64 = 0x10_0000;
const DESC: u64 = BASE;
const AVAIL: u64 = BASE + 0x1000;
const USED: u64 = BASE + 0x2000;
const HDRS: u64 = BASE + 0x3000;
const STATUS: u64 = BASE + 0x4000;
const DATA: u64 = BASE + 0x1_0000;
const MEM: usize = 0x80_0000;

/// A device behind a stand-in transport. The queue lock plays the
/// transport lock, and the kick path takes it as the real one does.
struct Rig {
    physmap: Arc<PhysMap>,
    queues: Mutex<Vec<VirtQueue>>,
    block: VirtioBlock,
    /// Cleared when the test resets the device: the transport refuses
    /// a kick on a ring the driver gave up.
    live: AtomicBool,
    /// Holds every request at the disk while set.
    hold: Arc<AtomicBool>,
}

impl Rig {
    /// One queue holding a read of each length in `lens`, all posted.
    fn new(lens: &[usize]) -> Arc<Self> {
        let physmap =
            Arc::new(PhysMap::new_anon(BASE, MEM).expect("guest memory"));
        let file = tempfile::tempfile().expect("create tempfile");
        file.set_len(lens.iter().sum::<usize>() as u64)
            .expect("size the backing file");
        let block = VirtioBlock::with_budget(
            file,
            &VirtioBlockOpts::default(),
            Arc::clone(&physmap),
            1,
            LIMIT,
        )
        .expect("create VirtioBlock");

        let mut queue = VirtQueue::new(RING);
        queue.set_addr_modern(DESC, AVAIL, USED);

        let mut data = DATA;
        let mut sector = 0u64;
        for (i, &len) in lens.iter().enumerate() {
            let hdr = HDRS + i as u64 * 16;
            physmap
                .lookup(hdr, BLK_REQ_HEADER_SIZE)
                .expect("mapped header")
                .write(&VirtioBlkReqHdr {
                    rtype: bits::VIRTIO_BLK_T_IN,
                    _reserved: 0,
                    sector,
                })
                .expect("write header");
            let first = 3 * i as u16;
            let descs = [
                VirtqDesc {
                    addr: hdr,
                    len: BLK_REQ_HEADER_SIZE as u32,
                    flags: bits::VRING_DESC_F_NEXT,
                    next: first + 1,
                },
                VirtqDesc {
                    addr: data,
                    len: len as u32,
                    flags: bits::VRING_DESC_F_NEXT | bits::VRING_DESC_F_WRITE,
                    next: first + 2,
                },
                VirtqDesc {
                    addr: STATUS + i as u64,
                    len: 1,
                    flags: bits::VRING_DESC_F_WRITE,
                    next: 0,
                },
            ];
            for (j, desc) in descs.iter().enumerate() {
                physmap
                    .lookup(DESC + u64::from(first + j as u16) * 16, 16)
                    .expect("mapped descriptor")
                    .write(desc)
                    .expect("write descriptor");
            }
            poke(&physmap, AVAIL + 4 + 2 * i as u64, &first.to_le_bytes());
            data += len as u64;
            sector += (len / 512) as u64;
        }
        poke(&physmap, AVAIL + 2, &(lens.len() as u16).to_le_bytes());

        let hold = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&hold);
        *block.ctx.parks.each_backend.lock().expect("park lock") =
            Some(Arc::new(move || {
                while held.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            }));

        let rig = Arc::new(Rig {
            physmap,
            queues: Mutex::new(vec![queue]),
            block,
            live: AtomicBool::new(true),
            hold,
        });
        let weak = Arc::downgrade(&rig);
        rig.block.install_kick(BackendKick::new(move |queue_idx| {
            if let Some(rig) = weak.upgrade() {
                rig.kick(queue_idx);
            }
        }));
        rig
    }

    /// What a guest write to the notify register does.
    fn kick(&self, queue_idx: u16) {
        let mut queues = self.queues.lock().expect("queue lock");
        if self.live.load(Ordering::Acquire) {
            self.block
                .notify_queue(queue_idx, &mut queues, &self.physmap);
        }
    }

    fn used(&self) -> u16 {
        self.queues.lock().expect("queue lock")[0]
            .read_used_ring_idx(&self.physmap)
    }

    fn taken(&self) -> u16 {
        self.queues.lock().expect("queue lock")[0].last_avail_idx()
    }

    fn in_flight(&self) -> (u64, usize) {
        self.block.ctx.budget.in_flight()
    }

    fn hold(&self, on: bool) {
        self.hold.store(on, Ordering::Release);
    }
}

#[test]
fn the_budget_refuses_past_its_limit_but_never_a_lone_request() {
    let budget = Budget::new(LIMIT);

    let big = budget.charge(0, 2 * LIMIT).expect("a lone request runs");
    assert!(
        budget.charge(0, 512).is_none(),
        "work joined a lone request"
    );
    drop(big);

    let charges: Vec<_> = (0..4)
        .map(|_| budget.charge(0, LIMIT / 4).expect("fits the budget"))
        .collect();
    assert!(budget.charge(0, 512).is_none(), "a full budget took more");
    assert!(
        budget.charge(0, 2 * LIMIT).is_none(),
        "a large request ran beside others"
    );
    assert_eq!(budget.in_flight(), (LIMIT, 4));
    drop(charges);
    assert_eq!(budget.in_flight(), (0, 0));
}

#[test]
fn a_starved_queue_is_due_once_half_the_budget_is_free() {
    let budget = Budget::new(LIMIT);
    let mut charges: Vec<_> = (0..4)
        .map(|_| budget.charge(0, LIMIT / 4).expect("fits the budget"))
        .collect();
    assert!(budget.charge(2, LIMIT / 4).is_none());

    charges.pop();
    assert_eq!(budget.take_kicks(), 0, "kicked with a quarter free");
    charges.pop();
    assert_eq!(budget.take_kicks(), 1 << 2, "half free, and no kick due");
    assert_eq!(budget.take_kicks(), 0, "one release kicked twice");

    // With nothing in flight no later release would come, so the last
    // one releases the queue whatever is free.
    let budget = Budget::new(LIMIT);
    let lone = budget.charge(0, 2 * LIMIT).expect("a lone request runs");
    assert!(budget.charge(1, 512).is_none());
    drop(lone);
    assert_eq!(budget.take_kicks(), 1 << 1);
}

#[test]
fn a_ring_past_the_budget_drains_without_a_guest_kick() {
    const N: usize = 12;
    let rig = Rig::new(&[REQ; N]);
    rig.hold(true);

    rig.kick(0);
    assert_eq!(rig.taken(), (LIMIT as usize / REQ) as u16);
    assert_eq!(rig.in_flight(), (LIMIT, LIMIT as usize / REQ));
    rig.kick(0);
    assert_eq!(rig.taken(), (LIMIT as usize / REQ) as u16, "took past it");

    // No guest kick from here on: the completions drain the ring.
    rig.hold(false);
    wait_for("the ring stalled behind the budget", || {
        rig.used() == N as u16
    });
    assert_eq!(rig.block.ctx.budget.peak(), (LIMIT, LIMIT as usize / REQ));
    for i in 0..N as u64 {
        assert_eq!(peek(&rig.physmap, STATUS + i), bits::VIRTIO_BLK_S_OK);
    }
}

#[test]
fn a_request_larger_than_the_budget_runs_alone() {
    let big = 2 * LIMIT as usize;
    let rig = Rig::new(&[big, big, 4096]);
    rig.hold(true);

    rig.kick(0);
    assert_eq!(rig.taken(), 1);

    rig.hold(false);
    wait_for("a request past the budget never ran", || rig.used() == 3);
    assert_eq!(rig.block.ctx.budget.peak(), (big as u64, 1));
}

#[test]
fn a_pause_waits_for_the_budget_alone() {
    const N: usize = 12;
    let taken = (LIMIT as usize / REQ) as u16;
    let rig = Rig::new(&[REQ; N]);
    rig.hold(true);
    rig.kick(0);
    assert_eq!(rig.taken(), taken);

    let paused = Instant::now();
    rig.block.pause();
    rig.hold(false);
    wait_for("the device did not quiesce", || rig.block.is_quiesced());
    assert!(
        paused.elapsed() < Duration::from_secs(2),
        "quiesce took {:?}",
        paused.elapsed()
    );

    // The pause waited for what was taken and took nothing more. The
    // rest is still the guest's, where a migration carries it.
    assert_eq!(rig.used(), taken);
    assert_eq!(rig.taken(), taken);
    assert_eq!(rig.in_flight(), (0, 0));
    rig.kick(0);
    assert_eq!(rig.taken(), taken, "a paused device took a chain");

    rig.block.resume();
    wait_for("resume left chains in the ring", || rig.used() == N as u16);
}

#[test]
fn a_reset_gives_the_budget_back() {
    let rig = Rig::new(&[REQ; 12]);
    rig.hold(true);
    rig.kick(0);
    assert_eq!(rig.in_flight(), (LIMIT, LIMIT as usize / REQ));

    rig.live.store(false, Ordering::Release);
    VirtioDevice::reset(&rig.block);
    rig.hold(false);

    // Budget kept by a dead session would starve the next driver.
    wait_for("the budget outlived the session", || {
        rig.in_flight() == (0, 0)
    });
    assert_eq!(rig.used(), 0, "a closed session published");
}

fn peek(physmap: &PhysMap, gpa: u64) -> u8 {
    let mut out = [0u8; 1];
    physmap
        .lookup(gpa, 1)
        .expect("mapped guest byte")
        .read_bytes(&mut out)
        .expect("read guest byte");
    out[0]
}
