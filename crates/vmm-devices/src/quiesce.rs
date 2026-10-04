// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt;
use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{
    mpsc, Arc, Condvar, Mutex, PoisonError, RwLock, RwLockReadGuard,
};
use std::thread;
use std::time::{Duration, Instant};

use crate::{FlushError, FlushIntent, Lifecycle};

const DURABLE_QUIESCE_BUDGET: Duration = Duration::from_secs(30);
const BEST_EFFORT_QUIESCE_BUDGET: Duration = Duration::from_secs(2);
const BEST_EFFORT_SYNC_BUDGET: Duration = Duration::from_secs(5);
/// How often a parked worker looks for work handed to it before the
/// pause.
const PARK_POLL: Duration = Duration::from_millis(50);

/// Shared pause/drain gate for devices with I/O worker threads.
///
/// WHY a bool predicate and not a future: no caller in this tree is async
/// (`bin/rshyve/src/reboot.rs`, `bin/rshyve/src/control.rs` both drive it from a
/// blocking thread), and a caller can only observe its own deadline if the
/// callee cooperates by yielding. A predicate makes the blocking-inside-poll
/// defect unrepresentable.
pub struct QuiesceGate {
    paused: AtomicBool,
    /// Drains in progress, one per live [`DrainScope`].
    ///
    /// WHY separate from `paused`: a drain parks the workers the same
    /// way a pause does, but the two have different owners. `paused`
    /// belongs to the control plane; a drain belongs to whoever holds
    /// the scope, which for a device reset is the guest. Keeping them
    /// apart is what stops a guest that loops DEVICE_STATUS writes from
    /// releasing a migration pause it never took.
    drains: AtomicUsize,
    /// Drains ever started.
    ///
    /// Whether a reset took the drain or the idle short cut is
    /// otherwise only visible as elapsed time, and a test that times a
    /// few atomics fails a correct tree on a loaded machine.
    drains_started: AtomicUsize,
    active: AtomicUsize,
    /// Held shared by each producer from its pause check until its work
    /// is handed to a worker, and held alone to start a pause or a
    /// drain. So once `pause` returns, no producer still holds work it
    /// decided to take before the pause.
    admission: RwLock<()>,
    lock: Mutex<()>,
    cv: Condvar,
}

/// A producer's permission to take work, from [`QuiesceGate::admit`].
pub struct Admission<'a> {
    _guard: RwLockReadGuard<'a, ()>,
}

/// One caller's claim on the workers, released when it is dropped.
///
/// Held across a drain that must not outlive its own scope: ending it
/// clears only this claim, never a pause or another drain.
pub struct DrainScope<'a> {
    gate: &'a QuiesceGate,
}

impl Drop for DrainScope<'_> {
    fn drop(&mut self) {
        let _guard = self.gate.lock.lock().expect("quiesce gate lock");
        self.gate.drains.fetch_sub(1, Ordering::AcqRel);
        self.gate.cv.notify_all();
    }
}

impl QuiesceGate {
    pub fn new(workers: usize) -> Self {
        Self {
            paused: AtomicBool::new(false),
            drains: AtomicUsize::new(0),
            drains_started: AtomicUsize::new(0),
            active: AtomicUsize::new(workers),
            admission: RwLock::new(()),
            lock: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    pub fn pause(&self) {
        let _admission = self.close_admission();
        self.paused.store(true, Ordering::Release);
    }

    /// Wait out every producer inside [`Self::admit`].
    fn close_admission(&self) -> std::sync::RwLockWriteGuard<'_, ()> {
        self.admission
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Permission to take work and hand it to a worker, or `None` while
    /// a pause or a drain holds the workers.
    ///
    /// Hold it until the work is in a worker's queue. A pause that
    /// starts in that window waits for it, so it never finds the
    /// workers parked with work still on the way to them.
    pub fn admit(&self) -> Option<Admission<'_>> {
        let guard = self
            .admission
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if self.is_paused() {
            return None;
        }
        Some(Admission { _guard: guard })
    }

    pub fn resume(&self) {
        let _guard = self.lock.lock().expect("quiesce gate lock");
        self.paused.store(false, Ordering::Release);
        self.cv.notify_all();
    }

    /// Park the workers for as long as the returned scope lives.
    ///
    /// Use this, not `pause`, for a drain the caller ends itself: a
    /// pause taken by another thread while the scope is held survives
    /// the drop.
    pub fn drain_scope(&self) -> DrainScope<'_> {
        let _admission = self.close_admission();
        self.drains_started.fetch_add(1, Ordering::Relaxed);
        self.drains.fetch_add(1, Ordering::AcqRel);
        DrainScope { gate: self }
    }

    /// How many drains have been started on this gate.
    pub fn drain_count(&self) -> usize {
        self.drains_started.load(Ordering::Relaxed)
    }

    /// Whether anything is holding the workers, a pause or a drain.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
            || self.drains.load(Ordering::Acquire) != 0
    }

    pub fn is_quiesced(&self) -> bool {
        self.active.load(Ordering::Acquire) == 0
    }

    pub fn park_if_paused(&self) {
        let _: Option<()> = self.park_if_paused_or(|| None);
    }

    /// Park as [`Self::park_if_paused`] does, but leave the park early
    /// with the work `poll` finds.
    ///
    /// Work can reach a parked worker: a worker that parked for one
    /// pause may still be parked when the next one starts, and work
    /// handed to it between the two waits there. The worker must
    /// finish it, or the pause waits for it for ever.
    pub fn park_if_paused_or<T>(
        &self,
        mut poll: impl FnMut() -> Option<T>,
    ) -> Option<T> {
        if !self.is_paused() {
            return None;
        }
        let mut guard = self.lock.lock().expect("quiesce gate lock");
        self.active.fetch_sub(1, Ordering::AcqRel);
        self.cv.notify_all();
        let mut found = None;
        while self.is_paused() {
            found = poll();
            if found.is_some() {
                break;
            }
            guard = self
                .cv
                .wait_timeout(guard, PARK_POLL)
                .expect("quiesce gate lock")
                .0;
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        found
    }

    pub fn wait_quiesced(&self, budget: Duration) -> bool {
        let guard = self.lock.lock().expect("quiesce gate lock");
        let (_guard, _result) = self
            .cv
            .wait_timeout_while(guard, budget, |_| {
                self.active.load(Ordering::Acquire) > 0
            })
            .expect("quiesce gate lock");
        self.active.load(Ordering::Acquire) == 0
    }
}

#[derive(Clone, Copy)]
struct FlushBudgets {
    durable_quiesce: Duration,
    best_effort_quiesce: Duration,
    best_effort_sync: Duration,
}

const FLUSH_BUDGETS: FlushBudgets = FlushBudgets {
    durable_quiesce: DURABLE_QUIESCE_BUDGET,
    best_effort_quiesce: BEST_EFFORT_QUIESCE_BUDGET,
    best_effort_sync: BEST_EFFORT_SYNC_BUDGET,
};

/// Quiesce a block backend and sync its file according to the caller's intent.
///
/// Returns an error without syncing if the workers miss their quiesce deadline.
/// Best-effort syncs also fail if their watchdog expires.
pub fn flush_file(
    gate: &QuiesceGate,
    file: &File,
    intent: FlushIntent,
    device: &'static str,
) -> Result<(), FlushError> {
    flush_file_with(gate, file, intent, device, FLUSH_BUDGETS, File::sync_data)
}

fn flush_file_with<F>(
    gate: &QuiesceGate,
    file: &File,
    intent: FlushIntent,
    device: &'static str,
    budgets: FlushBudgets,
    sync: F,
) -> Result<(), FlushError>
where
    F: FnOnce(&File) -> io::Result<()> + Send + 'static,
{
    // The flush owns this claim and nothing else: a pause another
    // thread takes while the sync runs must outlive it.
    let _drain = gate.drain_scope();

    let quiesce_budget = match intent {
        FlushIntent::Durable => budgets.durable_quiesce,
        FlushIntent::BestEffort => budgets.best_effort_quiesce,
    };
    if !gate.wait_quiesced(quiesce_budget) {
        return Err(FlushError::NotQuiesced(device));
    }

    let result = match intent {
        FlushIntent::Durable => sync(file).map_err(FlushError::Sync),
        FlushIntent::BestEffort => {
            let worker_file = match file.try_clone() {
                Ok(file) => file,
                Err(error) => return Err(FlushError::Sync(error)),
            };
            let (tx, rx) = mpsc::channel();
            let worker = thread::Builder::new()
                .name(format!("{device}-flush"))
                .spawn(move || {
                    // The receiver may have timed out, leaving no observer for
                    // the eventual sync result.
                    drop(tx.send(sync(&worker_file)));
                });
            match worker {
                Err(error) => Err(FlushError::Sync(error)),
                Ok(_handle) => {
                    match rx.recv_timeout(budgets.best_effort_sync) {
                        Ok(result) => result.map_err(FlushError::Sync),
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            Err(FlushError::TimedOut)
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            Err(FlushError::Sync(io::Error::other(
                                "device sync watchdog exited without a result",
                            )))
                        }
                    }
                }
            }
        }
    };

    result
}

/// A device, with the id its owner knows it by.
///
/// A type name alone cannot tell two virtio-blk devices apart.
#[derive(Clone)]
pub struct NamedDevice {
    pub id: Option<String>,
    pub device: Arc<dyn Lifecycle>,
}

/// One device that missed the quiesce deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckDevice {
    pub type_name: &'static str,
    /// Registry id, when the caller had one.
    pub id: Option<String>,
}

impl fmt::Display for StuckDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.id {
            Some(id) => write!(f, "{} ({id})", self.type_name),
            None => f.write_str(self.type_name),
        }
    }
}

pub struct QuiesceReport {
    /// Type names of the stuck devices, in wait order. Read
    /// [`devices`](Self::devices) to tell two of a type apart.
    pub stuck: Vec<&'static str>,
    devices: Vec<StuckDevice>,
}

impl QuiesceReport {
    /// Build a report from type names, for a caller that has no ids.
    pub fn new(stuck: Vec<&'static str>) -> Self {
        Self::from_devices(
            stuck
                .into_iter()
                .map(|type_name| StuckDevice {
                    type_name,
                    id: None,
                })
                .collect(),
        )
    }

    pub fn from_devices(devices: Vec<StuckDevice>) -> Self {
        let stuck = devices.iter().map(|dev| dev.type_name).collect();
        Self { stuck, devices }
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub fn devices(&self) -> &[StuckDevice] {
        &self.devices
    }

    /// Names for a log line: id when there is one, type name otherwise.
    pub fn names(&self) -> Vec<String> {
        self.devices.iter().map(StuckDevice::to_string).collect()
    }
}

/// Wait for one device, returning false when the deadline passed first.
fn wait_quiesced(dev: &dyn Lifecycle, deadline: Instant) -> bool {
    loop {
        if dev.is_quiesced() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5).min(deadline - now));
    }
}

/// Wait for every device against one shared deadline.
///
/// A per-device budget would serialize to the number of devices times the
/// budget.
pub fn wait_all_quiesced(
    devs: &[Arc<dyn Lifecycle>],
    budget: Duration,
) -> QuiesceReport {
    let deadline = Instant::now() + budget;
    let mut stuck = Vec::new();
    for dev in devs {
        if !wait_quiesced(dev.as_ref(), deadline) {
            stuck.push(StuckDevice {
                type_name: dev.type_name(),
                id: None,
            });
        }
    }
    QuiesceReport::from_devices(stuck)
}

/// The same wait, for a caller that can name each device.
pub fn wait_all_quiesced_named(
    devs: &[NamedDevice],
    budget: Duration,
) -> QuiesceReport {
    let deadline = Instant::now() + budget;
    let mut stuck = Vec::new();
    for named in devs {
        if !wait_quiesced(named.device.as_ref(), deadline) {
            stuck.push(StuckDevice {
                type_name: named.device.type_name(),
                id: named.id.clone(),
            });
        }
    }
    QuiesceReport::from_devices(stuck)
}

#[cfg(test)]
mod tests;
