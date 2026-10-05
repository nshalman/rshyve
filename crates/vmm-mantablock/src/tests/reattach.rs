// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Engine loss and reattach against an engine socket played by the test.

use std::io::{IoSlice, Read, Write};
use std::mem::MaybeUninit;
use std::net::Shutdown;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use manta_block_ring::control::{
    Attach, AttachOk, Message, ATTACH_DURABLE, PROTOCOL_VERSION,
};
use manta_block_ring::layout::{LAYOUT_VERSION, PAGE};
use manta_block_ring::{Geometry, OpId, Sqe};
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
use vmm_storage::nvme::backend::{BlockOp, BlockStatus};

use super::control::answer_pings;
use super::{job, pattern, setup_queues, Calls, Engine, TJob, BLOCKS, GEN};
use crate::conn::Conn;
use crate::{Config, Ring};

const DEPTH: u32 = 8;
const BUF_PAGES: u32 = 16;
const MAX_BLOCKS: u32 = 2;

/// A volatile ring, its first engine, and the socket a reattach
/// connects to.
struct Lab {
    ring: Arc<Ring<TJob>>,
    calls: Arc<Calls>,
    /// Queue 0 of the first engine; `more` has the other queues.
    eng: Engine,
    more: Vec<Engine>,
    /// The first engine's end of the control socket.
    ctl: UnixStream,
    listener: UnixListener,
    _dir: tempfile::TempDir,
}

fn lab(tune: impl FnOnce(&mut Config)) -> Lab {
    lab_n(1, tune)
}

fn lab_n(queues: u16, tune: impl FnOnce(&mut Config)) -> Lab {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("engine.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let pings = theirs.try_clone().unwrap();
    thread::spawn(move || answer_pings(pings));
    let calls = Arc::new(Calls::default());
    let (ring, mut engines) = setup_queues(
        queues,
        DEPTH,
        BUF_PAGES,
        MAX_BLOCKS,
        false,
        Arc::clone(&calls) as _,
        Some(Conn::new(ours).unwrap()),
        |c| {
            c.socket = path;
            c.allow_volatile = true;
            c.reattach = Duration::from_secs(5);
            tune(c);
        },
    );
    let eng = engines.remove(0);
    Lab {
        ring,
        calls,
        eng,
        more: engines,
        ctl: theirs,
        listener,
        _dir: dir,
    }
}

fn geometry() -> Geometry {
    Geometry::new(1, DEPTH, BUF_PAGES).unwrap()
}

fn grant(g: Geometry, generation: u64) -> AttachOk {
    AttachOk {
        generation,
        queues: g.queues(),
        depth: g.depth(),
        buf_pages: g.buf_pages(),
        max_blocks: MAX_BLOCKS,
        volume_blocks: BLOCKS,
        region_len: g.region_len() as u64,
        layout_version: LAYOUT_VERSION,
        flags: 0,
    }
}

impl Lab {
    /// Closes the control socket, as a dead engine does, and waits until
    /// the ring holds its I/O.
    fn lose(&self) {
        self.ctl.shutdown(Shutdown::Both).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !self.ring.is_held() {
            assert!(Instant::now() < until, "the ring did not hold");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Takes the next reattach, answers it with `ok` and a new engine of
    /// geometry `g`, and returns the Attach and that engine.
    fn accept(&self, g: Geometry, ok: AttachOk) -> (Attach, Engine) {
        let (ask, mut engines) = self.accept_all(g, ok);
        (ask, engines.remove(0))
    }

    /// As [`Lab::accept`], with an engine for each queue.
    fn accept_all(&self, g: Geometry, ok: AttachOk) -> (Attach, Vec<Engine>) {
        let until = Instant::now() + Duration::from_secs(5);
        let mut s = loop {
            match self.listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < until, "no reattach came");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("{e}"),
            }
        };
        s.set_nonblocking(false).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (engines, fds) = Engine::all(g, ok.generation);
        let ask = handshake(&mut s, ok, &fds);
        thread::spawn(move || answer_pings(s));
        (ask, engines)
    }
}

fn recv(s: &mut UnixStream, buf: &mut Vec<u8>) -> Message {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some((m, _, used)) = Message::decode(buf).unwrap() {
            buf.drain(..used);
            return m;
        }
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "the VMM closed the socket");
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// The engine's side of Hello and Attach, sending `fds` with AttachOk.
fn handshake(s: &mut UnixStream, ok: AttachOk, fds: &[OwnedFd]) -> Attach {
    let mut buf = Vec::new();
    assert!(matches!(recv(s, &mut buf), Message::Hello { .. }));
    let ack = Message::HelloAck {
        version: PROTOCOL_VERSION,
        features: 0,
    };
    s.write_all(&ack.encode(1)).unwrap();
    let Message::Attach(ask) = recv(s, &mut buf) else {
        panic!("no Attach");
    };
    let frame = Message::AttachOk(ok).encode(2);
    let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(|f| f.as_fd()).collect();
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8))];
    let mut anc = SendAncillaryBuffer::new(&mut space);
    assert!(anc.push(SendAncillaryMessage::ScmRights(&borrowed)));
    rustix::net::sendmsg(
        &*s,
        &[IoSlice::new(&frame)],
        &mut anc,
        SendFlags::empty(),
    )
    .unwrap();
    ask
}

fn seq(e: &Sqe) -> u64 {
    OpId::from_u128(e.op_id).seq
}

#[test]
fn an_engine_back_within_the_bound_gets_each_held_part_once_in_order() {
    let mut lab = lab(|_| {});
    // Done before the loss: never replayed.
    lab.ring.submit(job(
        0,
        0,
        BlockOp::Write { fua: false },
        60,
        pattern(1, 0),
    ));
    lab.eng.serve_all(1);
    lab.calls.wait_for(1);

    let w1 = job(1, 1, BlockOp::Write { fua: false }, 10, pattern(3, 1));
    let w1_mem = Arc::clone(&w1.mem);
    lab.ring.submit(w1);
    lab.ring.submit(job(2, 2, BlockOp::Flush, 0, Vec::new()));
    let r3 = Arc::new(Mutex::new(vec![0; PAGE]));
    lab.ring.submit(TJob {
        id: 3,
        queue: 1,
        tag: 3,
        op: BlockOp::Read,
        off: 40 * PAGE as u64,
        mem: Arc::clone(&r3),
    });
    lab.ring
        .submit(job(4, 4, BlockOp::Write { fua: true }, 50, pattern(1, 4)));
    // W1 as two parts, the flush, the read, W4's data part.
    let before = lab.eng.take_n(5);
    // The replay sends the data first sent, not what the guest has now.
    w1_mem.lock().unwrap().fill(0xee);

    lab.lose();
    lab.ring.submit(job(
        5,
        5,
        BlockOp::Write { fua: false },
        70,
        pattern(1, 5),
    ));
    thread::sleep(Duration::from_millis(50));
    assert!(lab.eng.take(8).is_empty(), "sent to a lost engine");
    assert!(lab.calls.all().len() == 1, "{:?}", lab.calls.all());

    let (ask, mut eng) = lab.accept(geometry(), grant(geometry(), GEN));
    assert_eq!(
        (ask.generation, ask.queues, ask.depth, ask.buf_pages),
        (GEN, 1, DEPTH, BUF_PAGES)
    );
    assert_eq!(ask.max_blocks, MAX_BLOCKS);
    eng.disk[40 * PAGE..41 * PAGE].copy_from_slice(&pattern(1, 3));

    let after = eng.take_n(6);
    let ids = |v: &[Sqe]| v.iter().map(|e| e.op_id).collect::<Vec<_>>();
    assert_eq!(ids(&after[..5]), ids(&before), "replays in op id order");
    let low = seq(&before[0]);
    for (a, b) in after.iter().zip(&before) {
        assert_eq!(
            (a.op, a.lba, a.blocks, a.buf_page, a.tag),
            (b.op, b.lba, b.blocks, b.buf_page, b.tag)
        );
        assert_eq!(a.watermark, low);
    }
    // The flush after both parts of the write before it.
    assert_eq!((after[0].op, after[1].op, after[2].op), (2, 2, 3));
    // What waited goes after the replays, with a new sequence.
    assert_eq!(after[5].lba, 70);
    assert!(seq(&after[5]) > seq(&before[4]));

    let cqes: Vec<_> = after.iter().map(|e| eng.serve(e).encode()).collect();
    eng.post(&cqes);
    // W4 is FUA on a volatile engine: its flush follows its data.
    let f = eng.take_n(1);
    assert_eq!(f[0].op, 3);
    let c = eng.serve(&f[0]).encode();
    eng.post(&[c]);

    let done = lab.calls.wait_for(6);
    let mut got: Vec<_> = done.iter().map(|(id, _)| *id).collect();
    got.sort_unstable();
    assert_eq!(got, (0..6).collect::<Vec<_>>(), "each request once");
    assert!(done.iter().all(|(_, s)| *s == BlockStatus::Success));
    assert_eq!(&eng.disk[10 * PAGE..13 * PAGE], &pattern(3, 1)[..]);
    assert_eq!(&eng.disk[50 * PAGE..51 * PAGE], &pattern(1, 4)[..]);
    assert_eq!(&eng.disk[70 * PAGE..71 * PAGE], &pattern(1, 5)[..]);
    assert_eq!(*r3.lock().unwrap(), pattern(1, 3));
    assert!(!lab.ring.is_held() && !lab.ring.is_lost());
    thread::sleep(Duration::from_millis(50));
    assert_eq!(lab.calls.all().len(), 6);
}

#[test]
fn held_io_fails_when_no_engine_comes_back_in_time() {
    let mut lab = lab(|c| c.reattach = Duration::from_millis(300));
    lab.ring.submit(job(1, 0, BlockOp::Read, 0, pattern(1, 0)));
    lab.eng.take_n(1);
    let t = Instant::now();
    lab.lose();
    // A request taken while held fails with the rest.
    lab.ring
        .submit(job(2, 1, BlockOp::Write { fua: false }, 3, pattern(1, 2)));
    let done = lab.calls.wait_for(2);
    assert!(
        t.elapsed() >= Duration::from_millis(250),
        "{:?}",
        t.elapsed()
    );
    assert!(
        done.iter().all(|(_, s)| *s == BlockStatus::Internal),
        "{done:?}"
    );
    assert!(lab.ring.is_lost());
    lab.ring.submit(job(3, 2, BlockOp::Read, 0, pattern(1, 0)));
    assert_eq!(lab.calls.wait_for(3)[2], (3, BlockStatus::Internal));
}

#[test]
fn a_replay_that_runs_past_the_bound_fails_held_io() {
    let mut lab = lab(|c| {
        c.reattach = Duration::from_millis(400);
        c.replay_stall = Duration::from_millis(600);
    });
    lab.ring
        .submit(job(1, 0, BlockOp::Write { fua: false }, 9, pattern(1, 9)));
    lab.eng.take_n(1);
    let t = Instant::now();
    lab.lose();
    let (_, mut eng) = lab.accept(geometry(), grant(geometry(), GEN));
    assert_eq!(lab.calls.wait_for(1), vec![(1, BlockStatus::Internal)]);
    assert!(
        t.elapsed() >= Duration::from_millis(400),
        "{:?}",
        t.elapsed()
    );
    assert!(lab.ring.is_lost() && !lab.ring.is_held());
    // Nothing more reaches the guest, and nothing new goes to the engine.
    let sent = eng.take(8).len();
    lab.ring.submit(job(2, 1, BlockOp::Read, 0, pattern(1, 0)));
    assert_eq!(lab.calls.wait_for(2)[1], (2, BlockStatus::Internal));
    assert!(sent <= 1);
    assert!(eng.take(8).is_empty());
    thread::sleep(Duration::from_millis(50));
    assert_eq!(lab.calls.all().len(), 2);
}

#[test]
fn a_flush_held_behind_writes_is_replayed_after_them() {
    let mut lab = lab(|_| {});
    for t in 0..3u16 {
        let lba = 8 * u64::from(t);
        let w = job(
            u32::from(t),
            t,
            BlockOp::Write { fua: false },
            lba,
            pattern(2, 7),
        );
        lab.ring.submit(w);
    }
    lab.ring.submit(job(3, 3, BlockOp::Flush, 0, Vec::new()));
    let before = lab.eng.take_n(4);
    lab.lose();
    let (_, mut eng) = lab.accept(geometry(), grant(geometry(), GEN));
    let after = eng.take_n(4);
    let ops: Vec<_> = after.iter().map(|e| e.op).collect();
    assert_eq!(ops, vec![2, 2, 2, 3]);
    let seqs: Vec<_> = after.iter().map(seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert_eq!(after[3].op_id, before[3].op_id);
    let cqes: Vec<_> = after.iter().map(|e| eng.serve(e).encode()).collect();
    eng.post(&cqes);
    let done = lab.calls.wait_for(4);
    assert!(done.iter().all(|(_, s)| *s == BlockStatus::Success));
}

#[test]
fn a_hostile_engine_on_reattach_is_refused() {
    let mut lab = lab(|_| {});
    lab.ring
        .submit(job(1, 0, BlockOp::Write { fua: false }, 9, pattern(1, 9)));
    lab.eng.take_n(1);
    lab.lose();

    // A smaller grant that Attach alone would take, then another
    // generation, then one region that claims more than was asked.
    let small = Geometry::new(1, DEPTH / 2, BUF_PAGES).unwrap();
    let big = Geometry::new(1, DEPTH * 2, BUF_PAGES).unwrap();
    let mut grant_durable = grant(geometry(), GEN);
    grant_durable.flags = ATTACH_DURABLE;
    let hostile = [
        (small, grant(small, GEN)),
        (geometry(), grant(geometry(), GEN + 1)),
        (big, grant(big, GEN)),
        (geometry(), grant_durable),
    ];
    let mut refused = Vec::new();
    for (g, ok) in hostile {
        let (ask, mut bad) = lab.accept(g, ok);
        assert_eq!(ask.generation, GEN);
        thread::sleep(Duration::from_millis(30));
        assert!(bad.take(8).is_empty(), "replayed into {g:?}");
        assert!(lab.ring.is_held());
        refused.push(bad);
    }
    assert!(lab.calls.all().is_empty(), "{:?}", lab.calls.all());

    let (_, mut eng) = lab.accept(geometry(), grant(geometry(), GEN));
    eng.serve_all(1);
    assert_eq!(lab.calls.wait_for(1), vec![(1, BlockStatus::Success)]);
    assert_eq!(&eng.disk[9 * PAGE..10 * PAGE], &pattern(1, 9)[..]);
    for mut bad in refused {
        assert!(bad.take(8).is_empty());
    }
}

#[test]
fn halt_while_held_fails_the_held_io_and_returns() {
    // An attempt waits for the engine up to the control deadline.
    let mut lab = lab(|c| c.deadline = Duration::from_millis(300));
    lab.ring.submit(job(1, 0, BlockOp::Read, 0, pattern(1, 0)));
    lab.eng.take_n(1);
    lab.lose();
    let t = Instant::now();
    lab.ring.halt();
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(lab.calls.all(), vec![(1, BlockStatus::Internal)]);
}

/// A write on guest queue `queue`.
fn write_on(id: u32, queue: u16, tag: u16, lba: u64) -> TJob {
    let mut j = job(id, tag, BlockOp::Write { fua: false }, lba, pattern(1, 3));
    j.queue = queue;
    j
}

#[test]
fn new_io_waits_until_every_queue_has_replayed() {
    let mut lab = lab_n(2, |c| c.replay_stall = Duration::from_millis(300));
    lab.ring.submit(write_on(1, 1, 0, 10));
    lab.ring.submit(write_on(2, 2, 0, 20));
    lab.eng.take_n(1);
    lab.more[0].take_n(1);
    lab.lose();
    let g = Geometry::new(2, DEPTH, BUF_PAGES).unwrap();
    let (_, mut engines) = lab.accept_all(g, grant(g, GEN));
    // Queue 0 has replayed; queue 1 is still in its slow copy.
    let first = engines[0].take_n(1);
    assert_eq!(first[0].lba, 10);
    lab.ring.submit(write_on(3, 1, 1, 30));
    thread::sleep(Duration::from_millis(100));
    assert!(engines[0].take(8).is_empty(), "new I/O before a replay");
    assert!(engines[1].take(8).is_empty());
    let second = engines[1].take_n(1);
    assert_eq!(second[0].lba, 20);
    let new = engines[0].take_n(1);
    assert_eq!(new[0].lba, 30);
    for (e, reqs) in [(0, [first, new].concat()), (1, second)] {
        let cqes: Vec<_> =
            reqs.iter().map(|r| engines[e].serve(r).encode()).collect();
        engines[e].post(&cqes);
    }
    let done = lab.calls.wait_for(3);
    assert!(done.iter().all(|(_, s)| *s == BlockStatus::Success));
}

#[test]
fn a_slow_replay_across_queues_fails_at_the_bound_not_after() {
    // Four parts, 150 ms each: the bound passes during the third.
    let mut lab = lab_n(2, |c| {
        c.reattach = Duration::from_millis(400);
        c.replay_stall = Duration::from_millis(150);
    });
    lab.ring.submit(write_on(1, 1, 0, 10));
    for t in 0..3u16 {
        lab.ring
            .submit(write_on(2 + u32::from(t), 2, t, 20 + u64::from(t)));
    }
    lab.eng.take_n(1);
    lab.more[0].take_n(3);
    let t = Instant::now();
    lab.lose();
    let g = Geometry::new(2, DEPTH, BUF_PAGES).unwrap();
    let (_, mut engines) = lab.accept_all(g, grant(g, GEN));
    let done = lab.calls.wait_for(4);
    let took = t.elapsed();
    assert!(
        done.iter().all(|(_, s)| *s == BlockStatus::Internal),
        "{done:?}"
    );
    assert!(took >= Duration::from_millis(400), "{took:?}");
    assert!(took < Duration::from_millis(560), "{took:?}");
    assert!(lab.ring.is_lost());
    // Queue 1 never published its replays.
    assert!(engines[1].take(8).is_empty());
}
