// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ring backend: its queues, its threads, and how it ends.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use vmm_devices::{FlushError, FlushIntent};
use vmm_storage::nvme::backend::{AsyncBackend, BlockRequest, BlockStatus};

use crate::attach::Attached;
use crate::conn::Conn;
use crate::job::{Job, Sink};
use crate::queue::{Lost, Out, QueueRing, Waiter};
use crate::{control, reaper, Config};

/// State the vCPU threads, the completion threads and the control thread
/// share.
pub(crate) struct Shared<J: Job> {
    pub(crate) queues: Vec<Mutex<QueueRing<J>>>,
    sink: Arc<dyn Sink<J::Id>>,
    lost: AtomicBool,
    /// Halt has started: a closed pipe or socket is then expected.
    halted: AtomicBool,
    /// The completion threads return.
    stop: AtomicBool,
    pub(crate) log: slog::Logger,
}

impl<J: Job> Shared<J> {
    pub(crate) fn queue(
        &self,
        q: usize,
    ) -> Option<MutexGuard<'_, QueueRing<J>>> {
        // A poisoned lock means a thread panicked inside the ring code;
        // its state is not to be trusted, so the queue counts as gone.
        self.queues.get(q)?.lock().ok()
    }

    /// Reports what finished. Never called with a queue lock held.
    pub(crate) fn finish(&self, mut out: Out<J::Id>) {
        if !out.done.is_empty() {
            self.sink.complete(&out.done);
        }
        out.wake_internal();
    }

    pub(crate) fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    pub(crate) fn halted(&self) -> bool {
        self.halted.load(Ordering::Acquire)
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.is_lost()
    }

    /// The engine is lost (spec 37 §8). Every request held fails.
    ///
    /// S2 has no remote path: spec 37 wants the requests kept and
    /// replayed over NVMe-oF to another engine (decision 6, option A),
    /// which is S5. Until then the guest sees an internal error.
    pub(crate) fn lose(&self, why: Lost) {
        if self.lost.swap(true, Ordering::AcqRel) {
            return;
        }
        slog::error!(self.log, "mantablock: engine lost, failing held I/O";
            "reason" => %why);
        self.fail_all();
    }

    fn fail_all(&self) {
        for q in 0..self.queues.len() {
            let mut out = Out::default();
            if let Some(mut g) = self.queue(q) {
                g.fail_all(&mut out);
            }
            self.finish(out);
        }
    }

    /// When the oldest request on any ring was sent.
    pub(crate) fn oldest(&self) -> Option<Instant> {
        (0..self.queues.len())
            .filter_map(|q| self.queue(q)?.oldest())
            .min()
    }
}

/// The NVMe backend over one MantaBlock attachment.
pub struct Ring<J: Job> {
    shared: Arc<Shared<J>>,
    /// The control thread, then one completion thread per queue.
    threads: Mutex<(Option<JoinHandle<()>>, Vec<JoinHandle<()>>)>,
    capacity: u64,
    depth: u16,
    durable: bool,
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
        let Attached {
            region,
            generation,
            volume_blocks,
            durable,
            slots,
            pipes,
        } = att;
        let depth = region.geometry().depth();
        let mut queues = Vec::new();
        let mut reapers = Vec::new();
        for (q, p) in (0u16..).zip(pipes) {
            let ring = QueueRing::new(
                q,
                Arc::clone(&region),
                p.wake_engine,
                slots,
                generation,
                depth,
                durable,
                volume_blocks,
            );
            let cq = region.cq_consumer(q);
            if let (Some(ring), Some(cq)) = (ring, cq) {
                queues.push(Mutex::new(ring));
                reapers.push((q, cq, p.wake_me));
            }
        }
        let shared = Arc::new(Shared {
            queues,
            sink,
            lost: AtomicBool::new(false),
            halted: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            log: cfg.log.clone(),
        });
        let mut cqs = Vec::new();
        let mut ctl = None;
        let mut failed = false;
        for (q, cq, wake_me) in reapers {
            let s = Arc::clone(&shared);
            let spin = cfg.spin;
            match thread::Builder::new()
                .name(format!("mantablock-cq-{q}"))
                .spawn(move || reaper::run(&s, q, cq, &wake_me, spin))
            {
                Ok(t) => cqs.push(t),
                Err(_) => failed = true,
            }
        }
        if let Some(conn) = conn {
            let s = Arc::clone(&shared);
            let c = control::Timers::from(cfg);
            match thread::Builder::new()
                .name("mantablock-ctl".into())
                .spawn(move || control::run(&s, conn, c))
            {
                Ok(t) => ctl = Some(t),
                Err(_) => failed = true,
            }
        }
        if failed {
            // `start` cannot fail: the controller is already built. A
            // ring without its threads would hang every request, so fail
            // them instead.
            shared.lose(Lost::Control("no thread for the ring".into()));
        }
        Arc::new(Self {
            shared,
            threads: Mutex::new((ctl, cqs)),
            capacity: volume_blocks * 4096,
            depth: slots.tags,
            durable,
            io_limit: cfg.io_limit,
            flushing: Mutex::new(()),
        })
    }

    /// Takes one request. Never blocks on I/O.
    pub fn submit(&self, job: J) {
        let mut out = Out::default();
        let q = usize::from(job.queue()).checked_sub(1);
        let lost = match q.and_then(|q| self.shared.queue(q)) {
            Some(mut g) => g.submit(job, &mut out).err(),
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

    /// True once the engine is lost.
    pub fn is_lost(&self) -> bool {
        self.shared.is_lost()
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
            Some(mut g) => g.flush(Arc::clone(&w), &mut out).err(),
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
        let (ctl, cqs) = match self.threads.lock() {
            Ok(mut t) => std::mem::take(&mut *t),
            Err(_) => (None, Vec::new()),
        };
        let me = thread::current().id();
        // The control thread goes first: it sends Detach, and the
        // completion threads take what the engine finishes meanwhile.
        if let Some(t) = ctl {
            join(t, me, &self.shared.log);
        }
        self.shared.stop.store(true, Ordering::Release);
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
