// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A file or zvol backend that a migration destination opens after the
//! commit. Until then it holds no descriptor, so the orchestrator can
//! receive the last zfs increment into the zvol.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use vmm_devices::blkdev::BackingDisk;
use vmm_devices::lifecycle::BackingAttach;
use vmm_devices::{FlushError, FlushIntent};

use super::backend::{
    AsyncBackend, BackendFactory, BlockRequest, BlockStatus, CompletionSink,
};
use super::bits::MAX_QUEUE_SIZE;
use super::io::IoBackend;
use super::pool::WorkerPool;

pub(super) struct LateFile {
    disk: BackingDisk,
    sink: Arc<dyn CompletionSink>,
    pool: RwLock<Option<WorkerPool>>,
    halted: AtomicBool,
    log: slog::Logger,
}

impl LateFile {
    pub(super) fn factory(
        disk: BackingDisk,
        log: slog::Logger,
    ) -> BackendFactory {
        Box::new(move |sink| {
            Ok(Arc::new(Self {
                disk,
                sink,
                pool: RwLock::new(None),
                halted: AtomicBool::new(false),
                log,
            }) as Arc<dyn AsyncBackend>)
        })
    }
}

impl AsyncBackend for LateFile {
    fn capacity(&self) -> u64 {
        self.disk.capacity()
    }

    fn queue_depth(&self) -> u16 {
        MAX_QUEUE_SIZE
    }

    fn attached(&self) -> bool {
        self.pool
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    fn attach(&self, how: &BackingAttach) -> Result<(), String> {
        if self.attached() {
            return Ok(());
        }
        if self.halted.load(Ordering::Acquire) {
            return Err("the disk is halted".into());
        }
        self.disk.attach(how.until).map_err(|e| e.to_string())?;
        let disk = Arc::clone(self.disk.get().ok_or("the disk did not open")?);
        let mut slot = self.pool.write().unwrap_or_else(|e| e.into_inner());
        // Checked under the lock a halt takes, so no pool outlives it.
        if self.halted.load(Ordering::Acquire) {
            return Err("the disk is halted".into());
        }
        if slot.is_none() {
            // After the commit this process holds the only copy of the
            // guest, so a thread that will not start fails the attach,
            // which can be tried again, and never aborts.
            let pool = WorkerPool::spawn(
                Arc::clone(&disk),
                disk as Arc<dyn IoBackend>,
                self.disk.capacity(),
                Arc::clone(&self.sink),
                self.log.clone(),
            )
            .map_err(|e| format!("nvme: the I/O workers did not start: {e}"))?;
            *slot = Some(pool);
        }
        Ok(())
    }

    fn submit(&self, req: BlockRequest) {
        let pool = self.pool.read().unwrap_or_else(|e| e.into_inner());
        match pool.as_ref() {
            Some(pool) => pool.submit(req),
            None => {
                drop(pool);
                // The controller keeps admission closed until the attach.
                slog::error!(self.log, "nvme: I/O before the disk opened";
                    "queue" => req.id().queue(), "tag" => req.id().tag());
                self.sink.complete(&[(req.id(), BlockStatus::Internal)]);
            }
        }
    }

    fn flush(&self, intent: FlushIntent) -> Result<(), FlushError> {
        match self.pool.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(pool) => pool.flush(intent),
            // Nothing was written to a disk that never opened.
            None => Ok(()),
        }
    }

    fn halt(&self) {
        self.halted.store(true, Ordering::Release);
        // Out of the lock: a worker that finishes during the stop
        // completes into the controller, which may submit.
        let pool = self.pool.write().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(pool) = pool {
            pool.halt();
        }
    }
}
