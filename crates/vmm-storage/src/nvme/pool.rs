// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The backing file or zvol behind the async interface.
//!
//! Its `preadv` and `pwritev` block, so a pool of threads runs them
//! straight between the store and the guest pages and reports each
//! request to the sink as it finishes.

use std::fs::File;
use std::io;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use vmm_devices::blkdev::DiskCache;
use vmm_devices::{FlushError, FlushIntent};

use super::backend::{
    AsyncBackend, BackendFactory, BlockOp, BlockRequest, BlockStatus,
    CompletionSink,
};
use super::bits::MAX_QUEUE_SIZE;
use super::io::IoBackend;

/// Matches the C bhyve blockif thread count.
pub(super) const NUM_WORKERS: usize = 8;

pub(super) struct WorkerPool {
    tx: Mutex<Option<mpsc::Sender<BlockRequest>>>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
    /// The store the workers move data through, synced for a flush.
    file: Arc<File>,
    capacity: u64,
    durable: bool,
    log: slog::Logger,
    /// Zvol write-cache control. Dropped after the workers stop, so
    /// the cache is put back only once nothing writes through it.
    _cache: DiskCache,
}

impl WorkerPool {
    /// A pool running `io`, whose store is `file`.
    ///
    /// The workers hold the only references to `io`, so the pool never
    /// keeps the store open past its own end.
    pub(super) fn factory(
        file: Arc<File>,
        io: Arc<dyn IoBackend>,
        read_only: bool,
        log: slog::Logger,
    ) -> io::Result<BackendFactory> {
        let capacity = file.metadata()?.len();
        Ok(Box::new(move |sink| {
            Arc::new(Self::spawn(file, io, read_only, capacity, sink, log))
                as Arc<dyn AsyncBackend>
        }))
    }

    fn spawn(
        file: Arc<File>,
        io: Arc<dyn IoBackend>,
        read_only: bool,
        capacity: u64,
        sink: Arc<dyn CompletionSink>,
        log: slog::Logger,
    ) -> Self {
        // NVMe has explicit FLUSH semantics, so the zvol may run
        // writeback. Set before any I/O so first writes hit the fast path.
        let cache = DiskCache::new(Arc::clone(&file), read_only);
        let durable = io.durable_writes();

        let (tx, rx) = mpsc::channel::<BlockRequest>();
        let rx = Arc::new(Mutex::new(rx));
        let workers = (0..NUM_WORKERS)
            .map(|i| {
                let rx = Arc::clone(&rx);
                let io = Arc::clone(&io);
                let sink = Arc::clone(&sink);
                thread::Builder::new()
                    .name(format!("nvme-io-{i}"))
                    .spawn(move || worker(&rx, io.as_ref(), sink.as_ref()))
                    .expect("nvme: failed to spawn I/O worker thread")
            })
            .collect();

        Self {
            tx: Mutex::new(Some(tx)),
            workers: Mutex::new(workers),
            file,
            capacity,
            durable,
            log,
            _cache: cache,
        }
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
        let tx = self.tx.lock().expect("nvme: pool tx lock");
        let sent = match tx.as_ref() {
            Some(tx) => tx.send(req).map_err(|error| error.0.id()),
            None => Err(req.id()),
        };
        if let Err(id) = sent {
            // The controller admits nothing after halt, so this is a
            // worker that died, and the request will never complete.
            slog::error!(self.log, "nvme: I/O dropped, the workers are gone";
                "queue" => id.queue(), "tag" => id.tag());
        }
    }

    fn flush(&self, intent: FlushIntent) -> Result<(), FlushError> {
        vmm_devices::quiesce::sync_file(&self.file, intent, "nvme")
    }

    fn halt(&self) {
        self.stop();
    }
}

fn worker(
    rx: &Mutex<mpsc::Receiver<BlockRequest>>,
    io: &dyn IoBackend,
    sink: &dyn CompletionSink,
) {
    loop {
        let req = {
            let rx = rx.lock().expect("nvme: pool rx lock");
            match rx.recv() {
                Ok(req) => req,
                Err(mpsc::RecvError) => return,
            }
        };
        let status = run(&req, io);
        sink.complete(&[(req.id(), status)]);
    }
}

/// Move one request's data between the store and the guest.
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
    }
}
