// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the in-flight budget.
//!
//! A pause waits for every request the device took, and the guest
//! picks how much it posts. The budget is what keeps that wait short,
//! and the kick is what keeps a ring it left work in from stalling.

use std::sync::atomic::{AtomicBool, AtomicU64};

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
/// The real policy cut to tenths: a 500 ms wait gives 300 ms.
const GRACE: PauseGrace = PauseGrace::new(
    Duration::from_millis(200),
    Duration::from_millis(100),
    Duration::from_millis(300),
);

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
    /// Bounce chunks that have reached the disk.
    chunks: Arc<AtomicUsize>,
    /// Milliseconds each chunk takes at the disk.
    slow_ms: Arc<AtomicU64>,
}

impl Rig {
    /// One queue holding a read of each length in `lens`, all posted.
    fn new(lens: &[usize]) -> Arc<Self> {
        Self::build(bits::VIRTIO_BLK_T_IN, lens)
    }

    /// One queue holding a request of type `rtype` for each length in
    /// `lens`.
    fn build(rtype: u32, lens: &[usize]) -> Arc<Self> {
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
            GRACE,
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
                    rtype,
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
                    flags: if rtype == bits::VIRTIO_BLK_T_IN {
                        bits::VRING_DESC_F_NEXT | bits::VRING_DESC_F_WRITE
                    } else {
                        bits::VRING_DESC_F_NEXT
                    },
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
        let chunks = Arc::new(AtomicUsize::new(0));
        let slow_ms = Arc::new(AtomicU64::new(0));
        let (held, seen, slow) =
            (Arc::clone(&hold), Arc::clone(&chunks), Arc::clone(&slow_ms));
        *block.ctx.parks.each_backend.lock().expect("park lock") =
            Some(Arc::new(move || {
                seen.fetch_add(1, Ordering::AcqRel);
                while held.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                thread::sleep(Duration::from_millis(
                    slow.load(Ordering::Acquire),
                ));
            }));

        let rig = Arc::new(Rig {
            physmap,
            queues: Mutex::new(vec![queue]),
            block,
            live: AtomicBool::new(true),
            hold,
            chunks,
            slow_ms,
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

    fn chunks(&self) -> usize {
        self.chunks.load(Ordering::Acquire)
    }

    fn slow(&self, ms: u64) {
        self.slow_ms.store(ms, Ordering::Release);
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
    assert!(budget.take_kicks().is_empty(), "kicked with a quarter free");
    charges.pop();
    assert_eq!(budget.take_kicks(), [2], "half free, and no kick due");
    assert!(budget.take_kicks().is_empty(), "one release kicked twice");

    // With nothing in flight no later release would come, so the last
    // one releases the queue whatever is free.
    let budget = Budget::new(LIMIT);
    let lone = budget.charge(0, 2 * LIMIT).expect("a lone request runs");
    assert!(budget.charge(1, 512).is_none());
    drop(lone);
    assert_eq!(budget.take_kicks(), [1]);
}

/// A FLUSH or a DISCARD names no data bytes. Without a floor on the
/// cost and a cap on the count, a guest could queue any number of them.
#[test]
fn work_with_no_data_bytes_is_still_bounded() {
    assert_eq!(budget::cost(bits::VIRTIO_BLK_T_FLUSH, 0), 0);
    let budget = Budget::new(budget::INFLIGHT_BYTES);
    let charges: Vec<_> = (0..budget::MAX_REQUESTS)
        .map(|_| {
            budget
                .charge(0, budget::cost(bits::VIRTIO_BLK_T_FLUSH, 0))
                .expect("under the cap")
        })
        .collect();
    assert_eq!(
        budget.in_flight(),
        (
            budget::MAX_REQUESTS as u64 * budget::MIN_COST,
            budget::MAX_REQUESTS
        )
    );
    assert!(budget.charge(0, 0).is_none(), "took past the request cap");
    assert!(
        budget.charge(0, 4096).is_none(),
        "took past the request cap"
    );
    drop(charges);
    assert_eq!(budget.in_flight(), (0, 0));
    assert_eq!(budget.take_kicks(), [0]);
}

/// A head larger than the budget runs only alone. Kicking its queue
/// while anything is in flight drains the ring only to give it back.
#[test]
fn a_head_past_the_budget_is_kicked_only_when_it_can_run() {
    let budget = Budget::new(LIMIT);
    let mut charges: Vec<_> = (0..4)
        .map(|_| budget.charge(0, LIMIT / 8).expect("fits the budget"))
        .collect();
    assert!(budget.charge(1, 2 * LIMIT).is_none());

    while charges.len() > 1 {
        charges.pop();
        assert!(
            budget.take_kicks().is_empty(),
            "kicked a head that cannot run yet"
        );
    }
    charges.pop();
    assert_eq!(budget.take_kicks(), [1]);
}

/// The queue kicked first gets the freed budget first, so the first
/// place takes turns.
#[test]
fn starved_queues_are_kicked_in_turn() {
    let budget = Budget::new(LIMIT);
    let mut firsts = Vec::new();
    for _ in 0..3 {
        let full = budget.charge(0, LIMIT).expect("an idle budget");
        for queue in 0..3 {
            assert!(budget.charge(queue, LIMIT / 4).is_none());
        }
        drop(full);
        let kicks = budget.take_kicks();
        assert_eq!(kicks.len(), 3, "a starved queue was left out");
        firsts.push(kicks[0]);
    }
    assert_eq!(firsts, [0, 1, 2], "one queue always went first");
}

/// Turns go round the queues that are due, not round queue numbers.
/// By number, the queue after a long gap would go first nearly every
/// time.
#[test]
fn sparse_starved_queues_take_turns() {
    let budget = Budget::new(LIMIT);
    let mut firsts = Vec::new();
    for _ in 0..4 {
        let full = budget.charge(0, LIMIT).expect("an idle budget");
        for queue in [0, 63] {
            assert!(budget.charge(queue, LIMIT / 4).is_none());
        }
        drop(full);
        let kicks = budget.take_kicks();
        assert_eq!(kicks.len(), 2, "a starved queue was left out");
        firsts.push(kicks[0]);
    }
    assert_eq!(firsts, [0, 63, 0, 63], "one queue always went first");
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

/// A pause that lands after the device took a request and before it
/// handed it to a worker. The pause is in force at once, so the
/// workers park while the request is still on its way to one of them.
/// The parked worker must take it and answer it, and the device must
/// not report quiet until it has: a migration that exported in that
/// window would carry a ring cursor past a request nobody answers.
#[test]
fn a_parked_worker_answers_a_request_that_reaches_it_late() {
    let rig = Rig::new(&[REQ]);
    let (at_tx, at_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    *rig.block
        .ctx
        .parks
        .before_dispatch
        .lock()
        .expect("park lock") = Some(Arc::new(move || {
        at_tx.send(()).expect("report the park");
        go_rx
            .lock()
            .expect("go lock")
            .recv()
            .expect("wait for the test");
    }));

    let kicker = {
        let rig = Arc::clone(&rig);
        thread::spawn(move || rig.kick(0))
    };
    at_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the request never reached the dispatch");
    // The kick holds the transport lock here, so the ring cursor is
    // read only once it returns.

    let (paused_tx, paused_rx) = mpsc::channel::<()>();
    let pauser = {
        let rig = Arc::clone(&rig);
        thread::spawn(move || {
            rig.block.pause();
            paused_tx.send(()).expect("report the pause");
        })
    };
    let gate = Arc::clone(&rig.block.gate);
    wait_for("the workers did not park", || gate.is_quiesced());
    assert!(
        paused_rx.try_recv().is_err(),
        "the pause returned with a request on its way to a worker"
    );
    assert!(!rig.block.is_quiesced(), "quiet with a request taken");

    // Held at the disk, the request keeps the device from quiet after
    // the parked worker has taken it.
    rig.hold(true);
    go_tx.send(()).expect("release the dispatch");
    paused_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the pause never returned");
    kicker.join().expect("kick thread");
    pauser.join().expect("pause thread");
    assert_eq!(rig.taken(), 1);
    wait_for("the parked worker left the request", || rig.chunks() == 1);
    thread::sleep(Duration::from_millis(100));
    assert!(!rig.block.is_quiesced(), "quiet with a request at the disk");

    rig.hold(false);
    wait_for("the device did not quiesce", || rig.block.is_quiesced());
    assert_eq!(rig.used(), 1, "quiet with the request unanswered");
    assert_eq!(peek(&rig.physmap, STATUS), bits::VIRTIO_BLK_S_OK);
    assert_eq!(rig.in_flight(), (0, 0));
    rig.block.resume();
}

/// The grace follows the caller's wait: a caller who waits long lets
/// one large request finish where a short wait would stop it.
#[test]
fn a_long_wait_lets_a_request_past_the_budget_finish() {
    const BIG: usize = 4 * LIMIT as usize;
    let rig = Rig::build(bits::VIRTIO_BLK_T_IN, &[BIG]);
    // 16 chunks of 40 ms, well past the grace a pause with no wait gets.
    rig.slow(40);
    rig.kick(0);
    wait_for("the request never reached the disk", || rig.chunks() > 0);

    rig.block.pause_within(Duration::from_secs(5));
    wait_for("the pause stopped a request inside the grace", || {
        rig.block.is_quiesced()
    });
    assert_eq!(rig.used(), 1);
    assert_eq!(rig.chunks(), BIG / REQ);
    assert_eq!(peek(&rig.physmap, STATUS), bits::VIRTIO_BLK_S_OK);
    assert_eq!(rig.in_flight(), (0, 0));
    rig.block.resume();
}

/// One that runs past a short wait's grace stops between chunks, still
/// owed, so the device does not report quiet and that pause fails.
/// After the resume the request ends, and the next pause succeeds.
#[test]
fn a_short_wait_holds_a_request_past_its_grace_until_a_retry() {
    const BIG: usize = 4 * LIMIT as usize;
    let rig = Rig::build(bits::VIRTIO_BLK_T_IN, &[BIG]);
    // 16 chunks of 25 ms: 400 ms if nothing stops it.
    rig.slow(25);
    rig.kick(0);
    wait_for("the request never reached the disk", || rig.chunks() > 0);

    let at_pause = rig.chunks();
    rig.block.pause_within(Duration::from_millis(300));
    thread::sleep(Duration::from_secs(1));
    let moved = rig.chunks();
    assert!(moved > at_pause, "the pause stopped it inside the grace");
    assert!(moved < BIG / REQ, "it ran on past the grace");
    assert_eq!(rig.used(), 0, "the request ran on through the pause");
    assert!(!rig.block.is_quiesced(), "quiet with a request half done");

    rig.block.resume();
    wait_for("the resume did not finish the request", || rig.used() == 1);
    assert_eq!(peek(&rig.physmap, STATUS), bits::VIRTIO_BLK_S_OK);
    rig.block.pause_within(Duration::from_millis(300));
    wait_for("the retry did not quiesce", || rig.block.is_quiesced());
    assert_eq!(rig.in_flight(), (0, 0));
    rig.block.resume();
}

#[test]
fn the_grace_leaves_the_caller_time_for_the_rest() {
    let grace = PauseGrace::DEFAULT;
    assert_eq!(grace.within(Duration::from_secs(5)), Duration::from_secs(3));
    assert_eq!(
        grace.within(Duration::from_secs(30)),
        Duration::from_secs(28)
    );
    assert_eq!(grace.within(Duration::from_secs(3)), Duration::from_secs(1));
    assert_eq!(
        grace.within(Duration::from_millis(20)),
        Duration::from_millis(10)
    );
    assert_eq!(grace.unknown(), Duration::from_secs(3));
}

/// A flush must not sync while a request past the budget is part way
/// through. A drain does not stop such a request, and the flush waits
/// for it to end.
#[test]
fn a_flush_waits_for_a_write_past_the_budget() {
    const BIG: usize = 4 * LIMIT as usize;
    let rig = Rig::build(bits::VIRTIO_BLK_T_OUT, &[BIG]);
    rig.slow(10);
    rig.kick(0);
    wait_for("the write never reached the disk", || rig.chunks() > 0);

    rig.block
        .flush_backing(FlushIntent::Durable)
        .expect("flush the disk");
    assert_eq!(rig.chunks(), BIG / REQ, "synced with the write half done");
    assert_eq!(rig.used(), 1);
    assert_eq!(peek(&rig.physmap, STATUS), bits::VIRTIO_BLK_S_OK);
}

/// A worker the drain parked can still take work that was on its way
/// to it. The flush must wait for that work too.
#[test]
fn a_flush_waits_for_work_a_parked_worker_takes() {
    let rig = Rig::new(&[REQ]);
    let (at_tx, at_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    *rig.block
        .ctx
        .parks
        .before_dispatch
        .lock()
        .expect("park lock") = Some(Arc::new(move || {
        at_tx.send(()).expect("report the park");
        go_rx
            .lock()
            .expect("go lock")
            .recv()
            .expect("wait for the test");
    }));
    let kicker = {
        let rig = Arc::clone(&rig);
        thread::spawn(move || rig.kick(0))
    };
    at_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the request never reached the dispatch");

    let (done_tx, done_rx) = mpsc::channel();
    let flusher = {
        let rig = Arc::clone(&rig);
        thread::spawn(move || {
            let flushed = rig.block.flush_backing(FlushIntent::Durable);
            done_tx.send(rig.used()).expect("report the flush");
            flushed
        })
    };
    let gate = Arc::clone(&rig.block.gate);
    wait_for("the drain did not park the workers", || gate.is_quiesced());

    // The workers stay parked with the request queued, so the gate
    // alone reads quiet.
    let skip = &rig.block.ctx.parks.parked_skips_work;
    skip.store(true, Ordering::Release);
    go_tx.send(()).expect("release the dispatch");
    kicker.join().expect("kick thread");
    assert!(
        done_rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "the flush synced with a request queued to a worker"
    );

    skip.store(false, Ordering::Release);
    assert_eq!(
        done_rx.recv_timeout(Duration::from_secs(5)),
        Ok(1),
        "the flush synced before the request was answered"
    );
    flusher
        .join()
        .expect("flush thread")
        .expect("flush the disk");
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
