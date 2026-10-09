// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ring backend: its queues, its threads, and how it ends.

use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use manta_block_ring::layout::CQE_BYTES;
use manta_block_ring::{Consumer, Region};
use vmm_devices::lifecycle::NvmeMigrateBackend;
use vmm_devices::{FlushError, FlushIntent};
use vmm_storage::nvme::backend::{AsyncBackend, BlockRequest, BlockStatus};

use crate::attach::Attached;
use crate::conn::Conn;
use crate::job::{Job, Sink};
use crate::out::{Lost, Out, Waiter};
use crate::queue::{QueueRing, Volume};
use crate::reattach::Plan;
use crate::{control, reaper, Config};

const LIVE: u8 = 0;
/// The engine is lost; requests wait for a reattach.
const HELD: u8 = 1;
/// The engine is lost for good; every request fails.
const DEAD: u8 = 2;

/// A completion thread to start: its queue, CQ and wake-up pipe.
pub(crate) type Reaper = (u16, Consumer<CQE_BYTES>, OwnedFd);

#[derive(Debug, Default)]
struct Phase {
    /// The engine was lost again while held, so a reattach in progress
    /// must not go live.
    again: bool,
}

pub(crate) struct Shared<J: Job> {
    pub(crate) queues: Vec<QueueRing<J>>,
    sink: Arc<dyn Sink<J::Id>>,
    /// LIVE, HELD or DEAD. Changed only under `phase`.
    state: AtomicU8,
    phase: Mutex<Phase>,
    /// A control thread is there to reattach.
    can_reattach: bool,
    /// A completion came, or nothing was held, since the last reattach.
    progress: AtomicBool,
    /// Halt has started: a closed pipe or socket is then expected.
    halted: AtomicBool,
    /// The completion threads return.
    stop: AtomicBool,
    /// The engine answered Detached.
    pub(crate) detached: AtomicBool,
    /// The completion threads of the current attachment.
    reapers: Mutex<Vec<JoinHandle<()>>>,
    spin: Duration,
    pub(crate) log: slog::Logger,
}

impl<J: Job> Shared<J> {
    pub(crate) fn queue(&self, q: usize) -> Option<&QueueRing<J>> {
        self.queues.get(q)
    }

    /// Reports what finished. Never called with a queue lock held.
    pub(crate) fn finish(&self, mut out: Out<J::Id>) {
        if !out.done.is_empty() {
            self.sink.complete(&out.done);
        }
        out.wake_internal();
    }

    pub(crate) fn is_lost(&self) -> bool {
        self.state.load(Ordering::Acquire) == DEAD
    }

    pub(crate) fn is_held(&self) -> bool {
        self.state.load(Ordering::Acquire) == HELD
    }

    pub(crate) fn halted(&self) -> bool {
        self.halted.load(Ordering::Acquire)
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
            || self.state.load(Ordering::Acquire) != LIVE
    }

    fn phase(&self) -> MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The engine is lost. With a control thread, every request is held
    /// for a reattach; else, or when a reattach cannot help, they fail.
    pub(crate) fn lose(&self, why: Lost) {
        let mut p = self.phase();
        match self.state.load(Ordering::Acquire) {
            DEAD => return,
            HELD if !why.fatal() => {
                p.again = true;
                return;
            }
            LIVE if self.can_reattach && !why.fatal() && !self.halted() => {
                // Lock order: `phase`, then each queue's `sq`.
                self.state.store(HELD, Ordering::Release);
                for q in &self.queues {
                    q.hold();
                }
                drop(p);
                slog::warn!(self.log, "mantablock: engine lost, holding I/O";
                    "reason" => %why);
                return;
            }
            _ => {}
        }
        self.state.store(DEAD, Ordering::Release);
        drop(p);
        slog::error!(self.log, "mantablock: engine lost, failing held I/O";
            "reason" => %why);
        self.fail_all();
    }

    /// Holds every queue again after a reattach that did not finish.
    pub(crate) fn rehold(&self) {
        let mut p = self.phase();
        p.again = false;
        for q in &self.queues {
            q.hold();
        }
    }

    /// Ends a reattach. False when the engine was lost again meanwhile:
    /// every queue is then held again.
    pub(crate) fn resume(&self, idle: bool) -> bool {
        let mut p = self.phase();
        if self.state.load(Ordering::Acquire) != HELD {
            return false;
        }
        if std::mem::take(&mut p.again) {
            for q in &self.queues {
                q.hold();
            }
            return false;
        }
        self.progress.store(idle, Ordering::Relaxed);
        self.state.store(LIVE, Ordering::Release);
        // Under `phase`, so a loss now holds them again after this.
        for q in &self.queues {
            q.release();
        }
        true
    }

    /// Whether a completion came, or nothing was held, since the last
    /// reattach. Clears it.
    pub(crate) fn take_progress(&self) -> bool {
        self.progress.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn progressed(&self) {
        if !self.progress.load(Ordering::Relaxed) {
            self.progress.store(true, Ordering::Relaxed);
        }
    }

    /// Starts a completion thread for each queue of `region`. False if
    /// one could not start.
    pub(crate) fn spawn_reapers(
        self: &Arc<Self>,
        region: &Arc<Region>,
        list: Vec<Reaper>,
    ) -> bool {
        let mut all = true;
        let mut threads =
            self.reapers.lock().unwrap_or_else(|e| e.into_inner());
        for (q, cq, wake_me) in list {
            let s = Arc::clone(self);
            let region = Arc::clone(region);
            let spin = self.spin;
            match thread::Builder::new()
                .name(format!("mantablock-cq-{q}"))
                .spawn(move || reaper::run(&s, q, cq, region, &wake_me, spin))
            {
                Ok(t) => threads.push(t),
                Err(_) => all = false,
            }
        }
        all
    }

    /// Waits until every completion thread has returned, and joins them.
    /// False if halt started first.
    pub(crate) fn quiesce(&self) -> bool {
        loop {
            {
                let mut t =
                    self.reapers.lock().unwrap_or_else(|e| e.into_inner());
                if t.iter().all(JoinHandle::is_finished) {
                    for t in t.drain(..) {
                        let _ = t.join();
                    }
                    return true;
                }
            }
            // Halt joins them itself, and may run on one of them.
            if self.halted() {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn fail_all(&self) {
        for q in &self.queues {
            let mut out = Out::default();
            q.fail_all(&mut out);
            self.finish(out);
        }
    }

    /// When the oldest request on any ring was sent, in monotonic ns.
    pub(crate) fn oldest(&self) -> Option<u64> {
        self.queues.iter().filter_map(QueueRing::oldest).min()
    }
}

/// The NVMe backend over one MantaBlock attachment.
pub struct Ring<J: Job> {
    shared: Arc<Shared<J>>,
    /// The control thread; the completion threads are in `shared`.
    ctl: Mutex<Option<JoinHandle<()>>>,
    capacity: u64,
    depth: u16,
    durable: bool,
    volume: uuid::Uuid,
    io_limit: Duration,
    /// One flush of the ring's own at a time.
    flushing: Mutex<()>,
}

impl<J: Job> Ring<J> {
    pub(crate) fn start(
        att: Attached,
        conn: Option<Conn>,
        cfg: &Config,
        sink: Arc<dyn Sink<J::Id>>,
    ) -> Arc<Self> {
        let plan = Plan::new(cfg, att.grant());
        let Attached {
            region,
            generation,
            volume_blocks,
            sector_bytes,
            zeroing,
            durable,
            slots,
            pipes,
            ..
        } = att;
        let volume = Volume {
            blocks: volume_blocks,
            sector: sector_bytes,
            zeroing,
        };
        let mut queues = Vec::new();
        let mut reapers = Vec::new();
        for (q, p) in (0u16..).zip(pipes) {
            let ring = QueueRing::new(
                q,
                Arc::clone(&region),
                p.wake_engine,
                slots,
                generation,
                durable,
                volume,
            );
            let cq = region.cq_consumer(q);
            if let (Some(ring), Some(cq)) = (ring, cq) {
                queues.push(ring);
                reapers.push((q, cq, p.wake_me));
            }
        }
        let shared = Arc::new(Shared {
            queues,
            sink,
            state: AtomicU8::new(LIVE),
            phase: Mutex::new(Phase::default()),
            can_reattach: conn.is_some() && !cfg.reattach.is_zero(),
            progress: AtomicBool::new(true),
            halted: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            detached: AtomicBool::new(false),
            reapers: Mutex::new(Vec::new()),
            spin: cfg.spin,
            log: cfg.log.clone(),
        });
        let mut ctl = None;
        let mut failed = !shared.spawn_reapers(&region, reapers);
        if let Some(conn) = conn {
            let s = Arc::clone(&shared);
            let c = control::Timers::from(cfg);
            match thread::Builder::new()
                .name("mantablock-ctl".into())
                .spawn(move || control::run(&s, conn, c, &plan))
            {
                Ok(t) => ctl = Some(t),
                Err(_) => failed = true,
            }
        }
        if failed {
            // `start` cannot fail: the controller is already built. A
            // ring without its threads would hang every request, so fail
            // them instead.
            shared.lose(Lost::Internal("no thread for the ring".into()));
        }
        Arc::new(Self {
            shared,
            ctl: Mutex::new(ctl),
            capacity: volume_blocks * 4096,
            depth: slots.tags,
            durable,
            volume: cfg.volume,
            io_limit: cfg.io_limit,
            flushing: Mutex::new(()),
        })
    }

    /// Takes one request. Never blocks on I/O.
    pub fn submit(&self, job: J) {
        let mut out = Out::default();
        let q = usize::from(job.queue()).checked_sub(1);
        let lost = match q.and_then(|q| self.shared.queue(q)) {
            Some(ring) => ring.submit(job, &mut out).err(),
            None => {
                out.done.push((job.id(), BlockStatus::Internal));
                None
            }
        };
        self.shared.finish(out);
        if let Some(why) = lost {
            self.shared.lose(why);
        }
    }

    /// Bytes the volume holds.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The volume, as a migration payload names it.
    pub fn identity(&self) -> NvmeMigrateBackend {
        NvmeMigrateBackend::MantaBlock {
            volume: self.volume,
        }
    }

    /// Always false: a ring alone cannot be handed over. A
    /// [`crate::Handover`] detaches it after the commit and attaches a
    /// new one on the destination.
    pub fn migratable(&self) -> bool {
        false
    }

    /// As [`Self::halt`], and says whether the engine confirmed the
    /// Detach. Requests still held fail.
    pub fn detach(&self) -> io::Result<()> {
        self.halt();
        if self.shared.detached.load(Ordering::Acquire) {
            return Ok(());
        }
        Err(io::Error::other("the engine did not confirm the Detach"))
    }

    /// Requests one queue may have in flight: one per buffer slot.
    pub fn queue_depth(&self) -> u16 {
        self.depth
    }

    /// Queues the engine granted.
    pub fn queues(&self) -> u16 {
        self.shared.queues.len() as u16
    }

    /// Whether a write is durable when it completes.
    pub fn durable(&self) -> bool {
        self.durable
    }

    /// True once the engine is lost for good.
    pub fn is_lost(&self) -> bool {
        self.shared.is_lost()
    }

    /// True while the engine is lost and I/O waits for a reattach.
    pub fn is_held(&self) -> bool {
        self.shared.is_held()
    }

    /// Makes every completed write durable. Only an engine whose writes
    /// are not durable needs a request for it.
    pub fn sync(&self) -> io::Result<()> {
        if self.durable {
            return Ok(());
        }
        let _one = self
            .flushing
            .lock()
            .map_err(|_| io::Error::other("flush lock poisoned"))?;
        let w = Arc::new(Waiter::default());
        let mut out = Out::default();
        let lost = match self.shared.queue(0) {
            Some(ring) => ring.flush(Arc::clone(&w), &mut out).err(),
            None => return Err(io::Error::other("no queue")),
        };
        self.shared.finish(out);
        if let Some(why) = lost {
            self.shared.lose(why);
        }
        match w.wait(Instant::now() + self.io_limit) {
            Some(BlockStatus::Success) => Ok(()),
            Some(s) => Err(io::Error::other(format!("flush: {s:?}"))),
            None => Err(io::ErrorKind::TimedOut.into()),
        }
    }

    /// Detaches and stops every thread. Requests still held fail.
    pub fn halt(&self) {
        if self.shared.halted.swap(true, Ordering::AcqRel) {
            return;
        }
        let ctl = self.ctl.lock().ok().and_then(|mut t| t.take());
        let me = thread::current().id();
        // The control thread goes first: it sends Detach, and the
        // completion threads take what the engine finishes meanwhile. It
        // starts no completion thread once it has returned.
        if let Some(t) = ctl {
            join(t, me, &self.shared.log);
        }
        self.shared.stop.store(true, Ordering::Release);
        let cqs = match self.shared.reapers.lock() {
            Ok(mut t) => std::mem::take(&mut *t),
            Err(e) => std::mem::take(&mut *e.into_inner()),
        };
        for t in cqs {
            join(t, me, &self.shared.log);
        }
        self.shared.fail_all();
    }
}

/// Joins `t`, unless it is this thread: a completion thread can hold the
/// last controller reference and run `halt` from its `Drop`.
fn join(t: JoinHandle<()>, me: thread::ThreadId, log: &slog::Logger) {
    if t.thread().id() == me {
        return;
    }
    if t.join().is_err() {
        slog::error!(log, "mantablock: a ring thread panicked");
    }
}

impl<J: Job> Drop for Ring<J> {
    fn drop(&mut self) {
        self.halt();
    }
}

impl AsyncBackend for Ring<BlockRequest> {
    fn capacity(&self) -> u64 {
        Ring::capacity(self)
    }

    fn queue_depth(&self) -> u16 {
        Ring::queue_depth(self)
    }

    fn max_queues(&self) -> u16 {
        self.queues()
    }

    fn durable_writes(&self) -> bool {
        self.durable
    }

    fn identity(&self) -> NvmeMigrateBackend {
        Ring::identity(self)
    }

    fn migratable(&self) -> bool {
        Ring::migratable(self)
    }

    fn submit(&self, req: BlockRequest) {
        Ring::submit(self, req);
    }

    fn flush(&self, _intent: FlushIntent) -> Result<(), FlushError> {
        self.sync().map_err(FlushError::Sync)
    }

    fn halt(&self) {
        Ring::halt(self);
    }
}
