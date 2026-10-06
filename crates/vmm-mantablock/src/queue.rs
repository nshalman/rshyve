// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One guest queue on its ring pair: the requests it holds, the parts it
//! has on the ring, and the checks every completion must pass.
//!
//! The submission side (`sq`) is the one producer of the SQ. The vCPU
//! takes it in `submit`; the completion thread takes it only to post what
//! waits for a free tag. The completion thread alone consumes the CQ.
//! Between the two there is the tag table, which needs no lock, and one
//! small lock per request, held to change it or move its data.
//!
//! Lock order: `sq`, then a request. Nothing calls the sink with either
//! held: the sink may call `submit` again on the same thread.
//!
//! While the engine is lost the queue is held: it takes requests but puts
//! nothing on the ring, and no completion thread runs. A reattach moves it
//! to the new region and replays the parts in flight.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use manta_block_ring::layout::{PAGE, SQE_BYTES};
use manta_block_ring::{Cqe, Op, Producer, Region, Sqe};
use vmm_storage::nvme::backend::{BlockOp, BlockStatus};

use crate::copy::{self, Bytes, Span, Stage};
use crate::ids::Seqs;
use crate::job::{block_status, Job};
use crate::out::{Lost, Out, Owner, Waiter};
use crate::shape::{data_span, part_bytes, zero_parts, ZeroPart};
use crate::split::Slots;
use crate::tags::{Part, Tags};
use crate::wake;

struct Req<J: Job> {
    owner: Owner<J>,
    op: Op,
    lba: u64,
    /// Blocks, or for a zeroing op its parts.
    blocks: u32,
    /// Where the guest range starts in the first block, and its bytes; 0
    /// bytes when it is every byte of the blocks.
    head: u32,
    len: u64,
    /// A zeroing op's ring parts.
    zero: Vec<ZeroPart>,
    /// Blocks already put on the ring.
    next: u32,
    /// Parts on the ring now.
    in_ring: u32,
    /// A flush still to send once the data parts are done: a flush
    /// itself, or a FUA write when writes are not durable.
    flush_after: bool,
    /// The first failure of any part.
    status: BlockStatus,
    /// Guest memory, for a read or write.
    pages: Option<J::Pages>,
    /// In the pending queue, or on its way there. The submission side
    /// then finishes it, not the completion thread.
    queued: bool,
}

/// The SQ producer and what only it uses.
struct SqSide {
    region: Arc<Region>,
    sq: Producer<SQE_BYTES>,
    wake_engine: OwnedFd,
    seqs: Seqs,
    /// The sequence each tag carries while it is not free.
    opened: Vec<u64>,
    /// The entry each tag carries while it is not free, for a replay.
    sent: Vec<Sqe>,
    /// Requests with something left to put on the ring, oldest first.
    pending: VecDeque<usize>,
    stage: Stage,
}

/// What the completion thread keeps between batches.
pub(crate) struct CqSide {
    region: Arc<Region>,
    stage: Stage,
    /// Tags to free, one bit each.
    freed: Vec<u64>,
    /// Requests to put in the pending queue.
    kicks: Vec<usize>,
}

/// What the queue checks a request against before it goes on the ring.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Volume {
    /// 4 KiB blocks.
    pub(crate) blocks: u64,
    /// The logical sector: 512 or 4096.
    pub(crate) sector: u32,
    /// The engine takes deallocate and write zeroes.
    pub(crate) zeroing: bool,
}

pub(crate) struct QueueRing<J: Job> {
    q: u16,
    slots: Slots,
    durable: bool,
    volume: Volume,
    sq: Mutex<SqSide>,
    tags: Tags,
    /// By guest tag; the last one is the ring's own.
    reqs: Vec<Mutex<Option<Req<J>>>>,
    /// The pending queue holds a request waiting for a free tag.
    waiting: AtomicBool,
    /// The engine is lost; nothing goes on the ring until a reattach.
    held: AtomicBool,
    dead: AtomicBool,
}

impl<J: Job> QueueRing<J> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        q: u16,
        region: Arc<Region>,
        wake_engine: OwnedFd,
        slots: Slots,
        generation: u64,
        durable: bool,
        volume: Volume,
    ) -> Option<Self> {
        let sq = region.sq_producer(q)?;
        let tags = usize::from(slots.tags);
        Some(Self {
            q,
            slots,
            durable,
            volume,
            sq: Mutex::new(SqSide {
                region,
                sq,
                wake_engine,
                seqs: Seqs::new(generation, q),
                opened: vec![0; tags],
                sent: vec![Sqe::decode(&[0; SQE_BYTES]); tags],
                pending: VecDeque::new(),
                stage: Stage::new(),
            }),
            tags: Tags::new(slots.tags),
            reqs: (0..=tags).map(|_| Mutex::new(None)).collect(),
            waiting: AtomicBool::new(false),
            held: AtomicBool::new(false),
            dead: AtomicBool::new(false),
        })
    }

    /// Ring tags in use.
    pub(crate) fn tags(&self) -> u16 {
        self.slots.tags
    }

    /// For a completion thread on `region`.
    pub(crate) fn cq_side(&self, region: Arc<Region>) -> CqSide {
        CqSide {
            region,
            stage: Stage::new(),
            freed: vec![0; self.tags.words()],
            kicks: Vec::new(),
        }
    }

    /// Takes one guest request. Never blocks on I/O: what does not fit on
    /// the ring now waits in the pending queue for a free tag.
    pub(crate) fn submit(
        &self,
        job: J,
        out: &mut Out<J::Id>,
    ) -> Result<(), Lost> {
        let slot = usize::from(job.tag());
        if slot >= usize::from(self.slots.tags)
            || self.dead.load(Ordering::Acquire)
        {
            // The controller admits at most the depth this ring reports.
            out.finish(Owner::Guest(job), BlockStatus::Internal);
            return Ok(());
        }
        let shape = match self.shape(&job) {
            Ok(Some(shape)) => shape,
            Ok(None) => {
                out.finish(Owner::Guest(job), BlockStatus::Success);
                return Ok(());
            }
            Err(s) => {
                out.finish(Owner::Guest(job), s);
                return Ok(());
            }
        };
        self.take(slot, Req::new(Owner::Guest(job), shape), out)
    }

    /// Sends a flush of the ring's own. One at a time.
    pub(crate) fn flush(
        &self,
        w: Arc<Waiter>,
        out: &mut Out<J::Id>,
    ) -> Result<(), Lost> {
        let slot = usize::from(self.slots.tags);
        self.take(slot, Req::new(Owner::Internal(w), Shape::flush()), out)
    }

    /// Holds `req` in `slot` and puts what it can on the ring.
    fn take(
        &self,
        slot: usize,
        mut req: Req<J>,
        out: &mut Out<J::Id>,
    ) -> Result<(), Lost> {
        let mut sq = lock(&self.sq)?;
        // `fail_all` sets `dead` under `sq`, so nothing is held after it.
        if self.dead.load(Ordering::Acquire) {
            out.finish(req.owner, BlockStatus::Internal);
            return Ok(());
        }
        {
            let mut held = lock(&self.reqs[slot])?;
            if held.is_some() {
                // Tags are unique in flight, and the ring's own flush
                // goes one at a time.
                drop(held);
                out.finish(req.owner, BlockStatus::Internal);
                return Ok(());
            }
            req.queued = true;
            *held = Some(req);
        }
        sq.pending.push_back(slot);
        self.pump(&mut sq, out)
    }

    /// The request in ring terms. `Ok(None)` when there is nothing to send.
    fn shape(&self, job: &J) -> Result<Option<Shape<J>>, BlockStatus> {
        let op = match job.op() {
            // Every completed write is already durable.
            BlockOp::Flush if self.durable => return Ok(None),
            BlockOp::Flush => return Ok(Some(Shape::flush())),
            BlockOp::Read => Op::Read,
            BlockOp::Write { .. } => Op::Write,
            BlockOp::Deallocate => return self.zeroing(job, Op::Deallocate),
            BlockOp::WriteZeroes { deallocate } => {
                let op = if deallocate {
                    Op::Deallocate
                } else {
                    Op::WriteZeroes
                };
                return self.zeroing(job, op);
            }
        };
        if job.is_empty() {
            return Ok(None);
        }
        let len = job.len() as u64;
        let Some(sp) = data_span(job.byte_offset(), len, self.volume.sector)
        else {
            return Err(BlockStatus::Internal);
        };
        // The engine detaches on a request past the end, so it is never
        // sent one.
        if sp
            .lba
            .checked_add(u64::from(sp.blocks))
            .is_none_or(|end| end > self.volume.blocks)
        {
            return Err(BlockStatus::LbaOutOfRange);
        }
        let pages = job.pages().map_err(|_| BlockStatus::DataTransfer)?;
        let fua = matches!(job.op(), BlockOp::Write { fua: true });
        Ok(Some(Shape {
            op,
            lba: sp.lba,
            blocks: sp.blocks,
            head: sp.head,
            len: sp.len,
            zero: Vec::new(),
            flush_after: fua && !self.durable,
            pages: Some(pages),
        }))
    }

    /// A deallocate or write zeroes: its ranges as ring parts, each
    /// checked against the volume as the engine checks it.
    fn zeroing(
        &self,
        job: &J,
        op: Op,
    ) -> Result<Option<Shape<J>>, BlockStatus> {
        if !self.volume.zeroing {
            return Err(BlockStatus::Unsupported);
        }
        let sector = u64::from(self.volume.sector);
        let mut zero = Vec::new();
        for (off, len) in job.zero_ranges() {
            if !off.is_multiple_of(sector) || !len.is_multiple_of(sector) {
                return Err(BlockStatus::Internal);
            }
            if off
                .checked_add(len)
                .is_none_or(|end| end > self.volume.blocks * PAGE as u64)
            {
                return Err(BlockStatus::LbaOutOfRange);
            }
            zero_parts(off, len, &mut zero);
        }
        if zero.is_empty() {
            return Ok(None);
        }
        let parts =
            u32::try_from(zero.len()).map_err(|_| BlockStatus::Internal)?;
        Ok(Some(Shape {
            op,
            lba: 0,
            blocks: parts,
            head: 0,
            len: 0,
            zero,
            flush_after: false,
            pages: None,
        }))
    }

    /// Puts what it can of the pending queue on the ring, oldest first,
    /// then wakes the engine if it sleeps.
    fn pump(&self, sq: &mut SqSide, out: &mut Out<J::Id>) -> Result<(), Lost> {
        if self.dead.load(Ordering::Acquire)
            || self.held.load(Ordering::Acquire)
        {
            return Ok(());
        }
        let mut posted = false;
        while let Some(&slot) = sq.pending.front() {
            let mut held = lock(&self.reqs[slot])?;
            let Some(req) = held.as_mut() else {
                sq.pending.pop_front();
                continue;
            };
            let ok = req.status == BlockStatus::Success;
            let data = ok && req.next < req.blocks;
            let flush = !data && ok && req.flush_after && req.in_ring == 0;
            if !data && !flush {
                req.queued = false;
                sq.pending.pop_front();
                settle(&mut held, out);
                continue;
            }
            let Some(tag) = self.tags.claim() else {
                drop(held);
                // Pairs with the fence in `complete`: either it sees
                // `waiting`, or this sees the tag it freed.
                self.waiting.store(true, Ordering::Relaxed);
                fence(Ordering::SeqCst);
                if self.tags.any_free() {
                    continue;
                }
                break;
            };
            match self.post(sq, slot, req, tag, flush) {
                Ok(()) => posted = true,
                Err(Posted::Copy(s)) => {
                    self.tags.unclaim(tag);
                    req.status = s;
                }
                Err(Posted::Lost(l)) => return Err(l),
            }
        }
        if sq.pending.is_empty() {
            self.waiting.store(false, Ordering::Relaxed);
        }
        if posted && sq.sq.publish() {
            wake::wake(&sq.wake_engine).map_err(|_| Lost::Pipe(self.q))?;
        }
        Ok(())
    }

    /// Puts one part of `req`, in `slot`, on the ring under `tag`.
    fn post(
        &self,
        sq: &mut SqSide,
        slot: usize,
        req: &mut Req<J>,
        tag: u16,
        flush: bool,
    ) -> Result<(), Posted> {
        let zeroing = !flush && !req.zero.is_empty();
        let page = if zeroing { 0 } else { self.slots.page(tag) };
        let (first, blocks) = if flush {
            (0, 0)
        } else if zeroing {
            (req.next, 1)
        } else {
            (req.next, self.slots.next_part(req.blocks, req.next))
        };
        let mut range = (0u16, 0u32);
        if !flush && req.op == Op::Write {
            let (Owner::Guest(job), Some(pages)) = (&req.owner, &req.pages)
            else {
                return Err(Posted::Copy(BlockStatus::Internal));
            };
            // The engine reads the buffer only after the tail moves, and
            // the tail store in `publish` releases these stores.
            if req.len == 0 {
                let span = Span {
                    q: self.q,
                    page,
                    first,
                    blocks,
                };
                copy::to_ring(&sq.region, job, pages, span, &mut sq.stage)
                    .map_err(Posted::Copy)?;
            } else {
                let b = part_bytes(&req.span(), first, blocks);
                let bytes = Bytes {
                    q: self.q,
                    buf: page as usize * PAGE + b.in_part,
                    at: b.at,
                    len: b.glen,
                };
                copy::to_ring_bytes(
                    &sq.region,
                    job,
                    pages,
                    bytes,
                    &mut sq.stage,
                )
                .map_err(Posted::Copy)?;
                range = (b.off, b.len);
            }
        }
        let zp = if zeroing {
            req.zero.get(first as usize).copied()
        } else {
            None
        };
        if let Some(z) = zp {
            range = (z.off, z.len);
        }
        let Ok(id) = sq.seqs.open() else {
            self.tags.abandon(tag);
            return Err(Posted::Lost(Lost::Exhausted(self.q)));
        };
        sq.opened[usize::from(tag)] = id.seq;
        let watermark = self.tags.oldest_open(&sq.opened).unwrap_or(id.seq);
        let queued_at = now_ns();
        // Before the push: a completion for this tag can come as soon as
        // the engine sees the entry.
        let part = Part {
            req: slot,
            first,
            blocks: if zeroing { 0 } else { blocks },
        };
        self.tags.open(tag, id.low(), part, queued_at);
        let (lba, part_blocks) = match zp {
            Some(z) => (z.lba, z.blocks),
            None if flush => (0, 0),
            None => (req.lba + u64::from(first), blocks),
        };
        let e = Sqe {
            op_id: id.to_u128(),
            lba,
            blocks: part_blocks,
            buf_page: if flush { 0 } else { page },
            watermark,
            queued_at,
            tag,
            op: if flush { Op::Flush } else { req.op } as u8,
            flags: 0,
            byte_off: range.0,
            byte_len: range.1,
            reserved: [0; 6],
        };
        let why = match sq.sq.try_push(&e.encode()) {
            Ok(true) => None,
            Ok(false) => Some(Lost::Full(self.q)),
            Err(_) => Some(Lost::Broken(self.q)),
        };
        if let Some(why) = why {
            self.tags.abandon(tag);
            return Err(Posted::Lost(why));
        }
        sq.sent[usize::from(tag)] = e;
        if flush {
            req.flush_after = false;
        } else {
            req.next += blocks;
        }
        req.in_ring += 1;
        Ok(())
    }

    /// Takes one batch from the CQ. A completion that matches no part in
    /// flight means the engine is lost.
    pub(crate) fn complete(
        &self,
        cqes: &[Cqe],
        cq: &mut CqSide,
        out: &mut Out<J::Id>,
    ) -> Result<(), Lost> {
        if self.dead.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut bad = None;
        for c in cqes {
            let part = match self.tags.complete(c) {
                Ok(part) => part,
                Err(e) => {
                    bad = Some(Lost::Completion(self.q, e));
                    break;
                }
            };
            self.land(c, part, cq, out)?;
            let t = usize::from(c.tag);
            cq.freed[t / 64] |= 1 << (t % 64);
        }
        self.tags.release(&mut cq.freed);
        fence(Ordering::SeqCst);
        // Even after a bad completion: a request landed before it may
        // wait for its flush, and a reattach sends it from `pending`.
        if !cq.kicks.is_empty() || self.waiting.load(Ordering::Relaxed) {
            let mut sq = lock(&self.sq)?;
            if self.dead.load(Ordering::Acquire) {
                cq.kicks.clear();
            } else {
                sq.pending.extend(cq.kicks.drain(..));
                self.pump(&mut sq, out)?;
            }
        }
        bad.map_or(Ok(()), Err)
    }

    /// One part is back: move its data and finish the request if it was
    /// the last.
    fn land(
        &self,
        c: &Cqe,
        part: Part,
        cq: &mut CqSide,
        out: &mut Out<J::Id>,
    ) -> Result<(), Lost> {
        let mut held = lock(&self.reqs[part.req])?;
        // `fail_all` may have finished it already.
        let Some(req) = held.as_mut() else {
            return Ok(());
        };
        req.in_ring -= 1;
        let mut s = block_status(c.status);
        if s == BlockStatus::Success && req.op == Op::Read && part.blocks > 0 {
            s = match (&req.owner, &req.pages) {
                (Owner::Guest(job), Some(pages)) => {
                    let page = self.slots.page(c.tag);
                    let r = if req.len == 0 {
                        let span = Span {
                            q: self.q,
                            page,
                            first: part.first,
                            blocks: part.blocks,
                        };
                        copy::from_ring(
                            &cq.region,
                            job,
                            pages,
                            span,
                            &mut cq.stage,
                        )
                    } else {
                        let b =
                            part_bytes(&req.span(), part.first, part.blocks);
                        let bytes = Bytes {
                            q: self.q,
                            buf: page as usize * PAGE + b.in_part,
                            at: b.at,
                            len: b.glen,
                        };
                        copy::from_ring_bytes(
                            &cq.region,
                            job,
                            pages,
                            bytes,
                            &mut cq.stage,
                        )
                    };
                    match r {
                        Ok(()) => BlockStatus::Success,
                        Err(s) => s,
                    }
                }
                _ => BlockStatus::Internal,
            };
        }
        if s != BlockStatus::Success && req.status == BlockStatus::Success {
            req.status = s;
        }
        if !req.queued && settle(&mut held, out) == Settled::Queue {
            cq.kicks.push(part.req);
        }
        Ok(())
    }

    /// The engine is lost: put nothing more on the ring. Under `sq`, so a
    /// submission already past the check finishes first.
    pub(crate) fn hold(&self) {
        let _sq = self.sq.lock().unwrap_or_else(|e| e.into_inner());
        self.held.store(true, Ordering::Release);
    }

    /// Moves the queue to a new attachment of the same grant. Replays
    /// each part in flight, oldest first, with its op id and, for a
    /// write, its data from the old region. The queue stays held: what
    /// waits goes in [`QueueRing::kick`]. Only while no completion thread
    /// runs on this queue. Returns how many parts and requests it held.
    pub(crate) fn reattach(
        &self,
        region: Arc<Region>,
        wake_engine: OwnedFd,
        budget: &Budget,
    ) -> Result<usize, Lost> {
        let mut sq = lock(&self.sq)?;
        if self.dead.load(Ordering::Acquire) {
            return Ok(0);
        }
        let producer =
            region.sq_producer(self.q).ok_or(Lost::Broken(self.q))?;
        let mut open = self.tags.after_loss();
        open.sort_by_key(|&t| sq.opened[usize::from(t)]);
        let old = std::mem::replace(&mut sq.region, region);
        sq.sq = producer;
        sq.wake_engine = wake_engine;
        let now = now_ns();
        let watermark = open.first().map_or(0, |&t| sq.opened[usize::from(t)]);
        // Every copy before any push: a reattach that fails after this
        // copies from the new region next time.
        for &tag in &open {
            budget.step()?;
            let SqSide {
                region,
                sent,
                stage,
                ..
            } = &mut *sq;
            let e = &mut sent[usize::from(tag)];
            e.watermark = watermark;
            e.queued_at = now;
            if e.op != Op::Write as u8 {
                continue;
            }
            let span = Span {
                q: self.q,
                page: e.buf_page,
                first: 0,
                blocks: e.blocks,
            };
            copy::between(&old, region, span, stage)
                .map_err(|_| Lost::Internal("replay copy".into()))?;
        }
        drop(old);
        for &tag in &open {
            let bytes = sq.sent[usize::from(tag)].encode();
            self.tags.repost(tag, now);
            match sq.sq.try_push(&bytes) {
                Ok(true) => {}
                Ok(false) => return Err(Lost::Full(self.q)),
                Err(_) => return Err(Lost::Broken(self.q)),
            }
        }
        if !open.is_empty() && sq.sq.publish() {
            wake::wake(&sq.wake_engine).map_err(|_| Lost::Pipe(self.q))?;
        }
        Ok(open.len() + sq.pending.len())
    }

    /// Ends a hold. Under `sq`, so a submission sees it or finds its
    /// request pumped by [`QueueRing::kick`].
    pub(crate) fn release(&self) {
        let _sq = self.sq.lock().unwrap_or_else(|e| e.into_inner());
        self.held.store(false, Ordering::Release);
    }

    /// Sends what waited while the queue was held.
    pub(crate) fn kick(&self, out: &mut Out<J::Id>) -> Result<(), Lost> {
        let mut sq = lock(&self.sq)?;
        self.pump(&mut sq, out)
    }

    /// The engine is lost for good: fail everything held, and take
    /// nothing more.
    pub(crate) fn fail_all(&self, out: &mut Out<J::Id>) {
        // A poisoned lock still guards what must fail.
        let mut sq = self.sq.lock().unwrap_or_else(|e| e.into_inner());
        self.dead.store(true, Ordering::Release);
        sq.pending.clear();
        for r in &self.reqs {
            let mut held = r.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(req) = held.take() {
                out.finish(req.owner, BlockStatus::Internal);
            }
        }
    }

    /// When the oldest part on the ring was sent, in monotonic ns.
    pub(crate) fn oldest(&self) -> Option<u64> {
        self.tags.oldest_posted()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Settled {
    Done,
    /// Its flush is still to send; it was marked queued.
    Queue,
    Waiting,
}

/// Finishes the request in `held` if nothing of it is left on the ring
/// or to send, or marks it for its flush.
fn settle<J: Job>(
    held: &mut MutexGuard<'_, Option<Req<J>>>,
    out: &mut Out<J::Id>,
) -> Settled {
    let Some(req) = held.as_mut() else {
        return Settled::Done;
    };
    let ok = req.status == BlockStatus::Success;
    if req.in_ring > 0 || (ok && req.next < req.blocks) {
        return Settled::Waiting;
    }
    if ok && req.flush_after {
        req.queued = true;
        return Settled::Queue;
    }
    if let Some(req) = held.take() {
        out.finish(req.owner, req.status);
    }
    Settled::Done
}

/// A poisoned lock means a thread panicked inside the ring code; its
/// state is not to be trusted, so the engine counts as lost.
fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>, Lost> {
    m.lock()
        .map_err(|_| Lost::Internal("queue lock poisoned".into()))
}

enum Posted {
    /// The data could not be moved; the request fails with this.
    Copy(BlockStatus),
    Lost(Lost),
}

/// A request in ring terms, before it has an owner.
struct Shape<J: Job> {
    op: Op,
    lba: u64,
    blocks: u32,
    head: u32,
    len: u64,
    zero: Vec<ZeroPart>,
    flush_after: bool,
    pages: Option<J::Pages>,
}

impl<J: Job> Shape<J> {
    /// A flush carries no data; it goes as one part once the queue
    /// reaches it.
    fn flush() -> Self {
        Self {
            op: Op::Flush,
            lba: 0,
            blocks: 0,
            head: 0,
            len: 0,
            zero: Vec::new(),
            flush_after: true,
            pages: None,
        }
    }
}

impl<J: Job> Req<J> {
    fn span(&self) -> crate::shape::Span {
        crate::shape::Span {
            lba: self.lba,
            blocks: self.blocks,
            head: self.head,
            len: self.len,
        }
    }

    fn new(owner: Owner<J>, s: Shape<J>) -> Self {
        Self {
            owner,
            op: s.op,
            lba: s.lba,
            blocks: s.blocks,
            head: s.head,
            len: s.len,
            zero: s.zero,
            next: 0,
            in_ring: 0,
            flush_after: s.flush_after,
            status: BlockStatus::Success,
            pages: s.pages,
            queued: false,
        }
    }
}

/// When a replay must be done.
pub(crate) struct Budget {
    pub(crate) until: Instant,
    /// Time each replayed part takes, as a slow copy would.
    #[cfg(test)]
    pub(crate) stall: std::time::Duration,
}

impl Budget {
    pub(crate) fn spent(&self) -> Result<(), Lost> {
        if Instant::now() >= self.until {
            return Err(Lost::Internal(
                "replay past the reattach bound".into(),
            ));
        }
        Ok(())
    }

    fn step(&self) -> Result<(), Lost> {
        #[cfg(test)]
        std::thread::sleep(self.stall);
        self.spent()
    }
}

/// Monotonic time in ns.
pub(crate) fn now_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (t.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(t.tv_nsec as u64)
}
