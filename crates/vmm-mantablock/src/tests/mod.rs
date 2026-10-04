// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ring against an engine played by the test, in this process, over
//! a real shared mapping and real pipes. The test engine can lie, which
//! the real one cannot be made to do.

#![allow(unsafe_code)]

use std::io;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use manta_block_ring::layout::{CQE_BYTES, PAGE, SQE_BYTES};
use manta_block_ring::{
    Consumer, Cqe, Geometry, Header, OpId, Producer, Region, Sqe, Status,
};
use vmm_storage::nvme::backend::{BlockOp, BlockStatus};

use crate::attach::{Attached, Pipes};
use crate::split::Slots;
use crate::wake::{adopt, End};
use crate::{Config, Job, Ring, Sink};

mod scatter;
mod stress;

const GEN: u64 = 0x51;
const BLOCKS: u64 = 256;

/// One request over its own guest buffer.
struct TJob {
    id: u32,
    tag: u16,
    op: BlockOp,
    off: u64,
    mem: Arc<Mutex<Vec<u8>>>,
}

impl Job for TJob {
    type Id = u32;
    type Pages = ();

    fn id(&self) -> u32 {
        self.id
    }

    fn queue(&self) -> u16 {
        1
    }

    fn tag(&self) -> u16 {
        self.tag
    }

    fn op(&self) -> BlockOp {
        self.op
    }

    fn byte_offset(&self) -> u64 {
        self.off
    }

    fn len(&self) -> usize {
        self.mem.lock().unwrap().len()
    }

    fn pages(&self) -> io::Result<()> {
        Ok(())
    }

    fn copy_from_guest(
        &self,
        _: &(),
        at: usize,
        dst: &mut [u8],
    ) -> io::Result<()> {
        dst.copy_from_slice(&self.mem.lock().unwrap()[at..at + dst.len()]);
        Ok(())
    }

    fn copy_to_guest(&self, _: &(), at: usize, src: &[u8]) -> io::Result<()> {
        self.mem.lock().unwrap()[at..at + src.len()].copy_from_slice(src);
        Ok(())
    }
}

/// Every call to the sink, as it came.
#[derive(Default)]
struct Calls(Mutex<Vec<Vec<(u32, BlockStatus)>>>);

impl Sink<u32> for Calls {
    fn complete(&self, done: &[(u32, BlockStatus)]) {
        self.0.lock().unwrap().push(done.to_vec());
    }
}

impl Calls {
    fn all(&self) -> Vec<(u32, BlockStatus)> {
        self.0.lock().unwrap().iter().flatten().copied().collect()
    }

    fn wait_for(&self, n: usize) -> Vec<(u32, BlockStatus)> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let all = self.all();
            if all.len() >= n {
                return all;
            }
            assert!(Instant::now() < until, "only {} of {n} done", all.len());
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn status(&self, id: u32) -> Option<BlockStatus> {
        self.all().iter().find(|(i, _)| *i == id).map(|(_, s)| *s)
    }
}

/// The engine's side: the same file mapped a second time.
struct Engine {
    region: Arc<Region>,
    sq: Consumer<SQE_BYTES>,
    cq: Producer<CQE_BYTES>,
    wake_me: OwnedFd,
    /// None once the test closes it, as a dead engine would.
    wake_vmm: Option<OwnedFd>,
    disk: Vec<u8>,
    seen: Vec<Sqe>,
}

fn map(file: &std::fs::File, g: Geometry) -> Arc<Region> {
    let m = memmap2::MmapOptions::new()
        .len(g.region_len())
        .map_raw(file)
        .unwrap();
    let base = m.as_mut_ptr();
    // SAFETY: a fresh shared mapping of `g.region_len()` bytes that the
    // Region owns from here on, reached only through it.
    Arc::new(unsafe { Region::with_owner(base, g.region_len(), g, m) }.unwrap())
}

/// A ring of one queue with `depth` entries and `buf_pages` pages, and
/// the test's engine for it.
fn setup(
    depth: u32,
    buf_pages: u32,
    max_blocks: u32,
    durable: bool,
) -> (Arc<Ring<TJob>>, Arc<Calls>, Engine) {
    setup_for(depth, buf_pages, max_blocks, durable)
}

fn setup_for<J: Job<Id = u32>>(
    depth: u32,
    buf_pages: u32,
    max_blocks: u32,
    durable: bool,
) -> (Arc<Ring<J>>, Arc<Calls>, Engine) {
    let calls = Arc::new(Calls::default());
    let (ring, engine) = setup_with(
        depth,
        buf_pages,
        max_blocks,
        durable,
        Arc::clone(&calls) as _,
    );
    (ring, calls, engine)
}

fn setup_with<J: Job<Id = u32>>(
    depth: u32,
    buf_pages: u32,
    max_blocks: u32,
    durable: bool,
    sink: Arc<dyn Sink<u32>>,
) -> (Arc<Ring<J>>, Engine) {
    let g = Geometry::new(1, depth, buf_pages).unwrap();
    let file = tempfile::tempfile().unwrap();
    file.set_len(g.region_len() as u64).unwrap();
    let theirs = map(&file, g);
    theirs.write_header(&Header {
        geometry: g,
        volume_blocks: BLOCKS,
        attach_generation: GEN,
    });
    let ours = map(&file, g);
    let (engine_rx, engine_tx) = rustix::pipe::pipe().unwrap();
    let (vmm_rx, vmm_tx) = rustix::pipe::pipe().unwrap();
    adopt(&engine_tx, End::Write).unwrap();
    adopt(&vmm_rx, End::Read).unwrap();
    adopt(&engine_rx, End::Read).unwrap();
    adopt(&vmm_tx, End::Write).unwrap();
    let att = Attached {
        region: Arc::clone(&ours),
        generation: GEN,
        volume_blocks: BLOCKS,
        durable,
        slots: Slots::new(depth, buf_pages, max_blocks).unwrap(),
        pipes: vec![Pipes {
            wake_engine: engine_tx,
            wake_me: vmm_rx,
        }],
    };
    let log = slog::Logger::root(slog::Discard, slog::o!());
    let mut cfg = Config::new("unused".into(), uuid::Uuid::nil(), log);
    cfg.spin = 10;
    let ring = Ring::start(att, None, &cfg, sink);
    let engine = Engine {
        sq: theirs.sq_consumer(0).unwrap(),
        cq: theirs.cq_producer(0).unwrap(),
        region: theirs,
        wake_me: engine_rx,
        wake_vmm: Some(vmm_tx),
        disk: vec![0; BLOCKS as usize * PAGE],
        seen: Vec::new(),
    };
    (ring, engine)
}

impl Engine {
    /// Takes every request on the ring, at most `max`.
    fn take(&mut self, max: usize) -> Vec<Sqe> {
        let mut got = Vec::new();
        while got.len() < max {
            match self.sq.pop().unwrap() {
                Some(b) => got.push(Sqe::decode(&b)),
                None => break,
            }
        }
        self.sq.release();
        self.seen.extend(got.iter().copied());
        got
    }

    /// Waits until `n` requests are on the ring, and takes them.
    fn take_n(&mut self, n: usize) -> Vec<Sqe> {
        let until = Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while got.len() < n {
            got.extend(self.take(n - got.len()));
            assert!(Instant::now() < until, "only {} of {n} sent", got.len());
            let _ = crate::wake::wait(&self.wake_me, Duration::from_millis(1));
        }
        got
    }

    /// Serves a request the way the real engine does.
    fn serve(&mut self, e: &Sqe) -> Cqe {
        let at = e.lba as usize * PAGE;
        let len = e.blocks as usize * PAGE;
        match e.op {
            1 => {
                let data = self.disk[at..at + len].to_vec();
                assert!(self.region.write_buffer(0, e.buf_page, &data));
            }
            2 => {
                let mut data = vec![0; len];
                assert!(self.region.read_buffer(0, e.buf_page, &mut data));
                self.disk[at..at + len].copy_from_slice(&data);
            }
            _ => {}
        }
        Cqe {
            tag: e.tag,
            status: Status::Success,
            op_seq: e.op_id as u64,
            engine_ns: 0,
            durable_ns: 0,
        }
    }

    /// Publishes completions in one batch.
    fn post(&mut self, cqes: &[[u8; CQE_BYTES]]) {
        for c in cqes {
            assert!(self.cq.try_push(c).unwrap());
        }
        if self.cq.publish() {
            if let Some(fd) = &self.wake_vmm {
                crate::wake::wake(fd).unwrap();
            }
        }
    }

    fn serve_all(&mut self, n: usize) {
        let reqs = self.take_n(n);
        let cqes: Vec<_> =
            reqs.iter().map(|e| self.serve(e).encode()).collect();
        self.post(&cqes);
    }
}

fn job(id: u32, tag: u16, op: BlockOp, lba: u64, data: Vec<u8>) -> TJob {
    TJob {
        id,
        tag,
        op,
        off: lba * PAGE as u64,
        mem: Arc::new(Mutex::new(data)),
    }
}

fn pattern(blocks: usize, seed: u8) -> Vec<u8> {
    (0..blocks * PAGE).map(|i| (i / 7) as u8 ^ seed).collect()
}

#[test]
fn writes_and_reads_round_trip_whole_and_split() {
    // Two pages per slot: a 5-block request goes as 3 parts.
    let (ring, calls, mut eng) = setup(8, 16, 2, true);
    assert_eq!(ring.queue_depth(), 8);
    let small = pattern(1, 3);
    let big = pattern(5, 9);
    ring.submit(job(1, 0, BlockOp::Write { fua: false }, 10, small.clone()));
    ring.submit(job(2, 1, BlockOp::Write { fua: true }, 20, big.clone()));
    eng.serve_all(4);
    calls.wait_for(2);
    assert_eq!(&eng.disk[10 * PAGE..11 * PAGE], &small[..]);
    assert_eq!(&eng.disk[20 * PAGE..25 * PAGE], &big[..]);

    let back = Arc::new(Mutex::new(vec![0; 5 * PAGE]));
    ring.submit(TJob {
        id: 3,
        tag: 0,
        op: BlockOp::Read,
        off: 20 * PAGE as u64,
        mem: Arc::clone(&back),
    });
    eng.serve_all(3);
    calls.wait_for(3);
    assert_eq!(*back.lock().unwrap(), big);
    assert!(calls.all().iter().all(|(_, s)| *s == BlockStatus::Success));

    // Every entry: this generation and queue, sequences that only go up,
    // a watermark at or below its own sequence, a buffer in its slot.
    let mut last = None;
    for e in &eng.seen {
        let id = OpId::from_u128(e.op_id);
        assert_eq!((id.generation, id.queue), (GEN, 0));
        assert!(last.is_none_or(|l| id.seq > l));
        last = Some(id.seq);
        assert!(e.watermark <= id.seq);
        assert!(e.blocks >= 1 && e.blocks <= 2);
        assert_eq!(e.buf_page, u32::from(e.tag) * 2);
    }
}

#[test]
fn completions_reaped_together_reach_the_sink_together() {
    let (ring, calls, mut eng) = setup(8, 8, 1, true);
    for t in 0..6u16 {
        ring.submit(job(
            u32::from(t),
            t,
            BlockOp::Read,
            u64::from(t),
            pattern(1, 0),
        ));
    }
    eng.serve_all(6);
    calls.wait_for(6);
    let batches = calls.0.lock().unwrap().clone();
    assert_eq!(batches.len(), 1, "{batches:?}");
}

#[test]
fn requests_beyond_the_free_tags_wait_and_then_go() {
    // Four tags of one page: three 3-block requests need nine parts.
    let (ring, calls, mut eng) = setup(4, 4, 1, true);
    for t in 0..3u16 {
        let data = pattern(3, t as u8);
        ring.submit(job(
            u32::from(t),
            t,
            BlockOp::Write { fua: false },
            30 * u64::from(t),
            data,
        ));
    }
    // Never more on the ring than the tags.
    let mut served = 0;
    let until = Instant::now() + Duration::from_secs(5);
    while served < 9 {
        assert!(Instant::now() < until, "only {served} of 9 parts sent");
        let got = eng.take(16);
        assert!(got.len() <= 4);
        let cqes: Vec<_> = got.iter().map(|e| eng.serve(e).encode()).collect();
        served += cqes.len();
        eng.post(&cqes);
        let _ = crate::wake::wait(&eng.wake_me, Duration::from_millis(5));
    }
    calls.wait_for(3);
    for t in 0..3usize {
        assert_eq!(
            &eng.disk[30 * t * PAGE..(30 * t + 3) * PAGE],
            &pattern(3, t as u8)[..]
        );
    }
}

#[test]
fn flush_is_free_when_writes_are_durable() {
    let (ring, calls, eng) = setup(8, 8, 1, true);
    ring.submit(job(7, 0, BlockOp::Flush, 0, Vec::new()));
    assert_eq!(calls.wait_for(1), vec![(7, BlockStatus::Success)]);
    assert!(ring.sync().is_ok());
    assert!(eng.seen.is_empty());
}

#[test]
fn flush_and_fua_go_to_a_volatile_engine() {
    let (ring, calls, mut eng) = setup(8, 8, 1, false);
    ring.submit(job(1, 0, BlockOp::Flush, 0, Vec::new()));
    let f = eng.take_n(1);
    assert_eq!((f[0].op, f[0].blocks, f[0].buf_page), (3, 0, 0));
    let c = eng.serve(&f[0]).encode();
    eng.post(&[c]);
    calls.wait_for(1);

    // A FUA write completes only after a flush that follows it.
    ring.submit(job(2, 1, BlockOp::Write { fua: true }, 3, pattern(1, 1)));
    let w = eng.take_n(1);
    assert_eq!(w[0].op, 2);
    let c = eng.serve(&w[0]).encode();
    eng.post(&[c]);
    let f = eng.take_n(1);
    assert_eq!(f[0].op, 3);
    assert_eq!(calls.status(2), None);
    let c = eng.serve(&f[0]).encode();
    eng.post(&[c]);
    calls.wait_for(2);
    assert_eq!(calls.status(2), Some(BlockStatus::Success));
}

#[test]
fn a_request_past_the_end_or_unaligned_never_reaches_the_ring() {
    let (ring, calls, eng) = setup(8, 8, 1, true);
    ring.submit(job(1, 0, BlockOp::Read, BLOCKS, pattern(1, 0)));
    let mut odd = job(2, 1, BlockOp::Read, 0, vec![0; 512]);
    odd.off = 512;
    ring.submit(odd);
    calls.wait_for(2);
    assert_eq!(calls.status(1), Some(BlockStatus::LbaOutOfRange));
    assert_eq!(calls.status(2), Some(BlockStatus::Internal));
    assert!(eng.seen.is_empty());
}

/// Sends one lying completion for the one request on the ring, and
/// checks that the ring treats the engine as lost.
fn lie(bytes: impl Fn(&Sqe) -> [u8; CQE_BYTES]) {
    let (ring, calls, mut eng) = setup(8, 8, 1, true);
    ring.submit(job(1, 0, BlockOp::Read, 0, pattern(1, 0)));
    ring.submit(job(2, 1, BlockOp::Write { fua: false }, 1, pattern(1, 0)));
    let reqs = eng.take_n(2);
    eng.post(&[bytes(&reqs[0])]);
    let done = calls.wait_for(2);
    assert!(
        done.iter().all(|(_, s)| *s == BlockStatus::Internal),
        "{done:?}"
    );
    assert!(ring.is_lost());
    // Nothing more is sent to a lost engine.
    ring.submit(job(3, 2, BlockOp::Read, 0, pattern(1, 0)));
    assert_eq!(calls.wait_for(3)[2], (3, BlockStatus::Internal));
    // Nor does a request that needs no ring succeed.
    ring.submit(job(4, 3, BlockOp::Flush, 0, Vec::new()));
    assert_eq!(calls.wait_for(4)[3], (4, BlockStatus::Internal));
    assert!(eng.take(8).is_empty());
}

#[test]
fn a_completion_with_the_wrong_op_id_loses_the_engine() {
    lie(|e| {
        let mut c = Cqe::decode(&[0; CQE_BYTES]).unwrap();
        c.tag = e.tag;
        c.op_seq = (e.op_id as u64) + 1;
        c.encode()
    });
}

#[test]
fn a_completion_for_a_tag_not_in_flight_loses_the_engine() {
    lie(|e| {
        let mut c = Cqe::decode(&[0; CQE_BYTES]).unwrap();
        c.tag = 7;
        c.op_seq = e.op_id as u64;
        c.encode()
    });
}

#[test]
fn a_completion_with_an_unknown_status_loses_the_engine() {
    lie(|e| {
        let mut c = Cqe::decode(&[0; CQE_BYTES]).unwrap();
        c.tag = e.tag;
        c.op_seq = e.op_id as u64;
        let mut b = c.encode();
        b[2] = 4;
        b
    });
}

#[test]
fn a_closed_wake_pipe_loses_the_engine() {
    let (ring, calls, mut eng) = setup(8, 8, 1, true);
    ring.submit(job(1, 0, BlockOp::Read, 0, pattern(1, 0)));
    eng.take_n(1);
    eng.wake_vmm = None;
    assert_eq!(calls.wait_for(1), vec![(1, BlockStatus::Internal)]);
    assert!(ring.is_lost());
}

#[test]
fn halt_fails_what_is_held_and_stops_the_threads() {
    let (ring, calls, mut eng) = setup(8, 8, 1, true);
    ring.submit(job(1, 0, BlockOp::Read, 0, pattern(1, 0)));
    eng.take_n(1);
    ring.halt();
    assert_eq!(calls.all(), vec![(1, BlockStatus::Internal)]);
    ring.halt();
}
