// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One guest queue on its ring pair: the requests it holds, the parts it
//! has on the ring, and the checks every completion must pass.
//!
//! The vCPU thread in `submit` and the queue's completion thread share
//! this state under one lock. Neither calls the sink with it held: the
//! sink may call `submit` again on the same thread.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use manta_block_ring::layout::{PAGE, SQE_BYTES};
use manta_block_ring::{
    BadCompletion, Cqe, Op, Outstanding, Producer, Region, Sqe,
};
use vmm_storage::nvme::backend::{BlockOp, BlockStatus};

use crate::copy::{self, Span, Stage};
use crate::ids::Seqs;
use crate::job::{block_status, Job};
use crate::split::Slots;
use crate::wake;

/// Why the engine counts as lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lost {
    /// An index moved in a way no correct engine moves it.
    Broken(u16),
    /// The SQ had no room for a request within the depth.
    Full(u16),
    /// A completion that matches no request in flight.
    Completion(u16, BadCompletion),
    /// A wake-up pipe closed or failed.
    Pipe(u16),
    /// The queue ran out of sequence numbers.
    Exhausted(u16),
    /// The control socket closed, failed or carried an Error.
    Control(String),
    /// No Pong within the heartbeat limit.
    Heartbeat,
    /// A request stayed on the ring past the I/O limit.
    Stalled,
}

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

/// A request the ring makes for itself, and the thread that waits on it.
#[derive(Debug, Default)]
pub(crate) struct Waiter {
    status: Mutex<Option<BlockStatus>>,
    cv: Condvar,
}

impl Waiter {
    fn set(&self, s: BlockStatus) {
        if let Ok(mut g) = self.status.lock() {
            *g = Some(s);
            self.cv.notify_all();
        }
    }

    /// Waits until the request ends or `until` passes.
    pub(crate) fn wait(&self, until: Instant) -> Option<BlockStatus> {
        let mut g = self.status.lock().ok()?;
        loop {
            if let Some(s) = *g {
                return Some(s);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            g = self.cv.wait_timeout(g, left).ok()?.0;
        }
    }
}

enum Owner<J> {
    Guest(J),
    Internal(Arc<Waiter>),
}

/// What finished under the lock, to report once it is released.
pub(crate) struct Out<Id> {
    pub(crate) done: Vec<(Id, BlockStatus)>,
    woken: Vec<(Arc<Waiter>, BlockStatus)>,
}

impl<Id> Default for Out<Id> {
    fn default() -> Self {
        Self {
            done: Vec::new(),
            woken: Vec::new(),
        }
    }
}

impl<Id> Out<Id> {
    fn finish<J: Job<Id = Id>>(&mut self, owner: Owner<J>, s: BlockStatus) {
        match owner {
            Owner::Guest(job) => self.done.push((job.id(), s)),
            Owner::Internal(w) => self.woken.push((w, s)),
        }
    }

    /// Wakes the ring's own waiters. The guest's go to the sink.
    pub(crate) fn wake_internal(&mut self) {
        for (w, s) in self.woken.drain(..) {
            w.set(s);
        }
    }
}

struct Req<J: Job> {
    owner: Owner<J>,
    op: Op,
    lba: u64,
    blocks: u32,
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
    /// In the pending queue.
    queued: bool,
}

struct Part {
    req: usize,
    first: u32,
    blocks: u32,
    seq: u64,
    posted: Instant,
}

pub(crate) struct QueueRing<J: Job> {
    q: u16,
    region: Arc<Region>,
    sq: Producer<SQE_BYTES>,
    wake_engine: OwnedFd,
    slots: Slots,
    seqs: Seqs,
    outstanding: Outstanding,
    /// By ring tag.
    parts: Vec<Option<Part>>,
    free: Vec<u16>,
    /// By guest tag; the last one is the ring's own.
    reqs: Vec<Option<Req<J>>>,
    /// Requests with something left to put on the ring, oldest first.
    pending: VecDeque<usize>,
    stage: Stage,
    durable: bool,
    volume_blocks: u64,
    dead: bool,
}

impl<J: Job> QueueRing<J> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        q: u16,
        region: Arc<Region>,
        wake_engine: OwnedFd,
        slots: Slots,
        generation: u64,
        depth: u32,
        durable: bool,
        volume_blocks: u64,
    ) -> Option<Self> {
        let sq = region.sq_producer(q)?;
        let tags = usize::from(slots.tags);
        Some(Self {
            q,
            region,
            sq,
            wake_engine,
            slots,
            seqs: Seqs::new(generation, q),
            outstanding: Outstanding::new(depth),
            parts: (0..tags).map(|_| None).collect(),
            free: (0..slots.tags).rev().collect(),
            reqs: (0..=tags).map(|_| None).collect(),
            pending: VecDeque::new(),
            stage: Stage::new(),
            durable,
            volume_blocks,
            dead: false,
        })
    }

    /// Takes one guest request. Never blocks: what does not fit on the
    /// ring now waits in the pending queue for a free tag.
    pub(crate) fn submit(
        &mut self,
        job: J,
        out: &mut Out<J::Id>,
    ) -> Result<(), Lost> {
        if self.dead {
            out.finish(Owner::Guest(job), BlockStatus::Internal);
            return Ok(());
        }
        let slot = usize::from(job.tag());
        if slot >= usize::from(self.slots.tags) || self.reqs[slot].is_some() {
            // The controller admits at most the depth this ring reports,
            // with tags unique in flight.
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
        self.queue(slot, Req::new(Owner::Guest(job), shape));
        self.pump(out)
    }

    /// Sends a flush of the ring's own. One at a time.
    pub(crate) fn flush(
        &mut self,
        w: Arc<Waiter>,
        out: &mut Out<J::Id>,
    ) -> Result<(), Lost> {
        let slot = usize::from(self.slots.tags);
        if self.dead || self.reqs[slot].is_some() {
            out.finish::<J>(Owner::Internal(w), BlockStatus::Internal);
            return Ok(());
        }
        self.queue(slot, Req::new(Owner::Internal(w), Shape::flush()));
        self.pump(out)
    }

    /// The request in ring terms. `Ok(None)` when there is nothing to send.
    fn shape(&self, job: &J) -> Result<Option<Shape<J>>, BlockStatus> {
        let op = match job.op() {
            // Every completed write is already durable.
            BlockOp::Flush if self.durable => return Ok(None),
            BlockOp::Flush => return Ok(Some(Shape::flush())),
            BlockOp::Read => Op::Read,
            BlockOp::Write { .. } => Op::Write,
        };
        if job.is_empty() {
            return Ok(None);
        }
        let len = job.len();
        let page = PAGE as u64;
        let off = job.byte_offset();
        // The engine serves 4 KiB blocks only.
        if !off.is_multiple_of(page) || !len.is_multiple_of(PAGE) {
            return Err(BlockStatus::Internal);
        }
        let lba = off / page;
        let blocks =
            u32::try_from(len / PAGE).map_err(|_| BlockStatus::Internal)?;
        // The engine detaches on a request past the end, so it is never
        // sent one.
        if lba
            .checked_add(u64::from(blocks))
            .is_none_or(|end| end > self.volume_blocks)
        {
            return Err(BlockStatus::LbaOutOfRange);
        }
        let pages = job.pages().map_err(|_| BlockStatus::DataTransfer)?;
        let fua = matches!(job.op(), BlockOp::Write { fua: true });
        Ok(Some(Shape {
            op,
            lba,
            blocks,
            flush_after: fua && !self.durable,
            pages: Some(pages),
        }))
    }

    fn queue(&mut self, slot: usize, mut req: Req<J>) {
        req.queued = true;
        self.reqs[slot] = Some(req);
        self.pending.push_back(slot);
    }

    /// Puts what it can of the pending queue on the ring, oldest first,
    /// then wakes the engine if it sleeps.
    fn pump(&mut self, out: &mut Out<J::Id>) -> Result<(), Lost> {
        let mut posted = false;
        while let Some(&slot) = self.pending.front() {
            let Some(req) = self.reqs[slot].as_mut() else {
                self.pending.pop_front();
                continue;
            };
            let data =
                req.next < req.blocks && req.status == BlockStatus::Success;
            let flush = !data
                && req.flush_after
                && req.in_ring == 0
                && req.status == BlockStatus::Success;
            if !data && !flush {
                req.queued = false;
                self.pending.pop_front();
                self.settle(slot, out);
                continue;
            }
            let Some(tag) = self.free.pop() else { break };
            match self.post(slot, tag, flush) {
                Ok(()) => posted = true,
                Err(Posted::Copy(s)) => {
                    self.free.push(tag);
                    if let Some(req) = self.reqs[slot].as_mut() {
                        req.status = s;
                    }
                }
                Err(Posted::Lost(l)) => return Err(l),
            }
        }
        if posted && self.sq.publish() {
            wake::wake(&self.wake_engine).map_err(|_| Lost::Pipe(self.q))?;
        }
        Ok(())
    }

    /// Puts one part of the request in `slot` on the ring under `tag`.
    fn post(
        &mut self,
        slot: usize,
        tag: u16,
        flush: bool,
    ) -> Result<(), Posted> {
        let Some(req) = self.reqs[slot].as_mut() else {
            return Err(Posted::Copy(BlockStatus::Internal));
        };
        let page = self.slots.page(tag);
        let (first, blocks) = if flush {
            (0, 0)
        } else {
            (req.next, self.slots.next_part(req.blocks, req.next))
        };
        if !flush && req.op == Op::Write {
            let (Owner::Guest(job), Some(pages)) = (&req.owner, &req.pages)
            else {
                return Err(Posted::Copy(BlockStatus::Internal));
            };
            let span = Span {
                q: self.q,
                page,
                first,
                blocks,
            };
            // The engine reads the buffer only after the tail moves, and
            // the tail store below releases these stores.
            copy::to_ring(&self.region, job, pages, span, &mut self.stage)
                .map_err(Posted::Copy)?;
        }
        let (id, watermark) = self
            .seqs
            .open()
            .map_err(|_| Posted::Lost(Lost::Exhausted(self.q)))?;
        let e = Sqe {
            op_id: id.to_u128(),
            lba: if flush { 0 } else { req.lba + u64::from(first) },
            blocks,
            buf_page: if flush { 0 } else { page },
            watermark,
            queued_at: now_ns(),
            tag,
            op: if flush { Op::Flush } else { req.op } as u8,
            flags: 0,
            reserved: [0; 12],
        };
        match self.sq.try_push(&e.encode()) {
            Ok(true) => {}
            Ok(false) => return Err(Posted::Lost(Lost::Full(self.q))),
            Err(_) => return Err(Posted::Lost(Lost::Broken(self.q))),
        }
        // Tags come from the free list, so the slot is always empty.
        if !self.outstanding.insert(tag, id) {
            return Err(Posted::Lost(Lost::Full(self.q)));
        }
        self.parts[usize::from(tag)] = Some(Part {
            req: slot,
            first,
            blocks,
            seq: id.seq,
            posted: Instant::now(),
        });
        if flush {
            req.flush_after = false;
        } else {
            req.next += blocks;
        }
        req.in_ring += 1;
        Ok(())
    }

    /// A completion that matches no part in flight means the engine is
    /// lost.
    pub(crate) fn complete(
        &mut self,
        cqes: &[Cqe],
        out: &mut Out<J::Id>,
    ) -> Result<(), Lost> {
        if self.dead {
            return Ok(());
        }
        for c in cqes {
            self.outstanding
                .complete(c)
                .map_err(|e| Lost::Completion(self.q, e))?;
            let tag = usize::from(c.tag);
            // `outstanding` holds exactly the tags `parts` does.
            let Some(part) = self.parts.get_mut(tag).and_then(Option::take)
            else {
                return Err(Lost::Completion(
                    self.q,
                    BadCompletion::Tag(c.tag),
                ));
            };
            self.free.push(c.tag);
            self.seqs.close(part.seq);
            self.land(c, &part, out);
        }
        self.pump(out)
    }

    /// One part is back: move its data and finish the request if it was
    /// the last.
    fn land(&mut self, c: &Cqe, part: &Part, out: &mut Out<J::Id>) {
        let Some(req) = self.reqs[part.req].as_mut() else {
            return;
        };
        req.in_ring -= 1;
        let mut s = block_status(c.status);
        if s == BlockStatus::Success && req.op == Op::Read && part.blocks > 0 {
            s = read_back(
                &self.region,
                self.q,
                self.slots,
                c.tag,
                part,
                req,
                &mut self.stage,
            );
        }
        if s != BlockStatus::Success && req.status == BlockStatus::Success {
            req.status = s;
        }
        if !req.queued {
            self.settle(part.req, out);
        }
    }

    /// Finishes the request in `slot` if nothing of it is left on the
    /// ring or to send, or queues its flush.
    fn settle(&mut self, slot: usize, out: &mut Out<J::Id>) {
        let Some(req) = self.reqs[slot].as_mut() else {
            return;
        };
        let ok = req.status == BlockStatus::Success;
        if req.in_ring > 0 || (ok && req.next < req.blocks) {
            return;
        }
        if ok && req.flush_after {
            if !req.queued {
                req.queued = true;
                self.pending.push_back(slot);
            }
            return;
        }
        let Some(req) = self.reqs[slot].take() else {
            return;
        };
        if req.queued {
            self.pending.retain(|&s| s != slot);
        }
        out.finish(req.owner, req.status);
    }

    /// The engine is lost: fail everything held, and take nothing more.
    pub(crate) fn fail_all(&mut self, out: &mut Out<J::Id>) {
        self.dead = true;
        self.pending.clear();
        for p in &mut self.parts {
            *p = None;
        }
        for r in &mut self.reqs {
            if let Some(req) = r.take() {
                out.finish(req.owner, BlockStatus::Internal);
            }
        }
    }

    /// When the oldest part on the ring was sent.
    pub(crate) fn oldest(&self) -> Option<Instant> {
        self.parts.iter().flatten().map(|p| p.posted).min()
    }
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
            flush_after: true,
            pages: None,
        }
    }
}

impl<J: Job> Req<J> {
    fn new(owner: Owner<J>, s: Shape<J>) -> Self {
        Self {
            owner,
            op: s.op,
            lba: s.lba,
            blocks: s.blocks,
            next: 0,
            in_ring: 0,
            flush_after: s.flush_after,
            status: BlockStatus::Success,
            pages: s.pages,
            queued: false,
        }
    }
}

/// Copies a finished read part out of the ring to the guest.
fn read_back<J: Job>(
    region: &Region,
    q: u16,
    slots: Slots,
    tag: u16,
    part: &Part,
    req: &Req<J>,
    stage: &mut Stage,
) -> BlockStatus {
    let (Owner::Guest(job), Some(pages)) = (&req.owner, &req.pages) else {
        return BlockStatus::Internal;
    };
    let span = Span {
        q,
        page: slots.page(tag),
        first: part.first,
        blocks: part.blocks,
    };
    match copy::from_ring(region, job, pages, span, stage) {
        Ok(()) => BlockStatus::Success,
        Err(s) => s,
    }
}

/// Monotonic time in ns, for the engine's probes only.
fn now_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (t.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(t.tv_nsec as u64)
}
