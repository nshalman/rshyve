// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The backing file or zvol behind the async interface.
//!
//! Its `preadv` and `pwritev` block, so a pool of threads runs them
//! straight between the store and the guest pages and reports each
//! request to the sink as it finishes.

use std::io;
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender};
use vmm_devices::blkdev::DiskSync;
use vmm_devices::{FlushError, FlushIntent};

use super::backend::{
    AsyncBackend, BackendFactory, BlockOp, BlockRequest, BlockStatus,
    CompletionSink,
};
use super::bits::MAX_QUEUE_SIZE;
use super::io::IoBackend;

/// Matches the C bhyve blockif thread count.
pub(super) const NUM_WORKERS: usize = 8;

#[cfg(test)]
thread_local! {
    /// Fail the spawn of this worker on this thread.
    // Some clippy releases flag a `const` initializer as not const.
    #[allow(clippy::missing_const_for_thread_local)]
    pub(super) static FAIL_SPAWN: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

fn spawn_worker(
    i: usize,
    body: impl FnOnce() + Send + 'static,
) -> io::Result<thread::JoinHandle<()>> {
    #[cfg(test)]
    if FAIL_SPAWN.with(|f| f.get()) == Some(i) {
        return Err(io::Error::other("no thread for the test"));
    }
    thread::Builder::new()
        .name(format!("nvme-io-{i}"))
        .spawn(body)
}

pub(super) struct WorkerPool {
    tx: Mutex<Option<Sender<BlockRequest>>>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
    /// Dropped after the workers stop, so a zvol write cache is turned
    /// back off only once nothing writes through it.
    disk: Arc<DiskSync>,
    capacity: u64,
    durable: bool,
    sink: Arc<dyn CompletionSink>,
    log: slog::Logger,
}

impl WorkerPool {
    /// The pool keeps no `io` of its own, so the store closes once the
    /// workers and `disk` are gone.
    pub(super) fn factory(
        disk: Arc<DiskSync>,
        io: Arc<dyn IoBackend>,
        log: slog::Logger,
    ) -> io::Result<BackendFactory> {
        let capacity = disk.file().metadata()?.len();
        Ok(Box::new(move |sink| {
            Ok(Arc::new(Self::spawn(disk, io, capacity, sink, log)?)
                as Arc<dyn AsyncBackend>)
        }))
    }

    /// Fails when a worker thread cannot start. The ones that started
    /// are stopped again, so nothing is left running.
    pub(super) fn spawn(
        disk: Arc<DiskSync>,
        io: Arc<dyn IoBackend>,
        capacity: u64,
        sink: Arc<dyn CompletionSink>,
        log: slog::Logger,
    ) -> io::Result<Self> {
        let durable = io.durable_writes();

        let (tx, rx) = crossbeam_channel::unbounded::<BlockRequest>();
        let mut workers = Vec::with_capacity(NUM_WORKERS);
        for i in 0..NUM_WORKERS {
            let rx = rx.clone();
            let io = Arc::clone(&io);
            let sink = Arc::clone(&sink);
            let started = spawn_worker(i, move || {
                worker(&rx, io.as_ref(), sink.as_ref())
            });
            match started {
                Ok(handle) => workers.push(handle),
                Err(e) => {
                    // Dropping the sender ends the workers already up.
                    drop(tx);
                    for handle in workers {
                        if handle.join().is_err() {
                            slog::error!(log, "nvme: I/O worker panicked");
                        }
                    }
                    slog::error!(log, "nvme: an I/O worker did not start";
                        "error" => %e);
                    return Err(e);
                }
            }
        }

        Ok(Self {
            tx: Mutex::new(Some(tx)),
            workers: Mutex::new(workers),
            disk,
            capacity,
            durable,
            sink,
            log,
        })
    }

    /// Disconnect the channel and wait for the workers.
    ///
    /// Dropping the sender is the only thing that ends a worker.
    fn stop(&self) {
        drop(self.tx.lock().expect("nvme: pool tx lock").take());
        let workers = std::mem::take(
            &mut *self.workers.lock().expect("nvme: pool workers lock"),
        );
        // A worker that held the last controller reference runs this
        // from `Drop`, so its own handle is in the list. Joining the
        // running thread would deadlock; it is on its way out either way.
        let me = thread::current().id();
        for worker in workers {
            if worker.thread().id() == me {
                continue;
            }
            if worker.join().is_err() {
                slog::error!(self.log, "nvme: I/O worker panicked");
            }
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.stop();
    }
}

impl AsyncBackend for WorkerPool {
    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn queue_depth(&self) -> u16 {
        MAX_QUEUE_SIZE
    }

    fn durable_writes(&self) -> bool {
        self.durable
    }

    fn submit(&self, req: BlockRequest) {
        let sent = match self.tx.lock().expect("nvme: pool tx lock").as_ref() {
            Some(tx) => tx.send(req).map_err(|error| error.0.id()),
            None => Err(req.id()),
        };
        if let Err(id) = sent {
            // No worker will ever take it, and its tag stays taken until
            // it completes.
            slog::error!(self.log, "nvme: I/O failed, the workers are gone";
                "queue" => id.queue(), "tag" => id.tag());
            self.sink.complete(&[(id, BlockStatus::Internal)]);
        }
    }

    fn flush(&self, intent: FlushIntent) -> Result<(), FlushError> {
        vmm_devices::quiesce::sync_file(&self.disk, intent, "nvme")
    }

    fn halt(&self) {
        self.stop();
    }
}

fn worker(
    rx: &Receiver<BlockRequest>,
    io: &dyn IoBackend,
    sink: &dyn CompletionSink,
) {
    while let Ok(req) = rx.recv() {
        let status = run(&req, io);
        sink.complete(&[(req.id(), status)]);
    }
}

fn run(req: &BlockRequest, io: &dyn IoBackend) -> BlockStatus {
    // A reset or a Delete Queue since the submission took the pages
    // back. The controller drops whatever this reports.
    if !req.data().is_live() {
        return BlockStatus::DataTransfer;
    }
    let len = req.data().len();
    match req.op() {
        BlockOp::Read => {
            let Ok(iov) = req.data().iovec() else {
                return BlockStatus::DataTransfer;
            };
            match io.read_at(&iov, req.byte_offset()) {
                Ok(n) if n == len => BlockStatus::Success,
                _ => BlockStatus::DataTransfer,
            }
        }
        BlockOp::Write { fua } => {
            let Ok(iov) = req.data().iovec() else {
                return BlockStatus::DataTransfer;
            };
            match io.write_at(&iov, req.byte_offset()) {
                Ok(n) if n == len => {}
                _ => return BlockStatus::DataTransfer,
            }
            if fua && io.sync().is_err() {
                return BlockStatus::Internal;
            }
            BlockStatus::Success
        }
        BlockOp::Flush => match io.sync() {
            Ok(()) => BlockStatus::Success,
            Err(_) => BlockStatus::Internal,
        },
        // The controller offers these only to a backend that takes them.
        BlockOp::Deallocate | BlockOp::WriteZeroes { .. } => {
            BlockStatus::Unsupported
        }
    }
}
