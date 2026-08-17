// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::{FlushError, FlushIntent, Lifecycle};

const DURABLE_QUIESCE_BUDGET: Duration = Duration::from_secs(30);
const BEST_EFFORT_QUIESCE_BUDGET: Duration = Duration::from_secs(2);
const BEST_EFFORT_SYNC_BUDGET: Duration = Duration::from_secs(5);

/// Platform-appropriate data sync for block-device flush operations.
#[cfg(target_os = "illumos")]
pub(crate) fn datasync(fd: i32) -> i32 {
    unsafe { libc::fdatasync(fd) }
}

#[cfg(not(target_os = "illumos"))]
pub(crate) fn datasync(fd: i32) -> i32 {
    unsafe { libc::fsync(fd) }
}

/// Shared pause/drain gate for devices with I/O worker threads.
///
/// WHY a bool predicate and not a future: no caller in this tree is async
/// (`bin/vmm/src/reboot.rs`, `bin/vmm/src/control.rs` both drive it from a
/// blocking thread), and a caller can only observe its own deadline if the
/// callee cooperates by yielding. A predicate makes the blocking-inside-poll
/// defect unrepresentable.
pub struct QuiesceGate {
    paused: AtomicBool,
    active: AtomicUsize,
    lock: Mutex<()>,
    cv: Condvar,
}

impl QuiesceGate {
    pub fn new(workers: usize) -> Self {
        Self {
            paused: AtomicBool::new(false),
            active: AtomicUsize::new(workers),
            lock: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        let _guard = self.lock.lock().expect("quiesce gate lock");
        self.paused.store(false, Ordering::Release);
        self.cv.notify_all();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub fn is_quiesced(&self) -> bool {
        self.active.load(Ordering::Acquire) == 0
    }

    pub fn park_if_paused(&self) {
        if !self.paused.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self.lock.lock().expect("quiesce gate lock");
        self.active.fetch_sub(1, Ordering::AcqRel);
        self.cv.notify_all();
        while self.paused.load(Ordering::Acquire) {
            guard = self.cv.wait(guard).expect("quiesce gate lock");
        }
        self.active.fetch_add(1, Ordering::AcqRel);
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
    flush_file_with(gate, file, intent, device, FLUSH_BUDGETS, |file| {
        if datasync(file.as_raw_fd()) == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    })
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
    let was_paused = gate.is_paused();
    gate.pause();

    let quiesce_budget = match intent {
        FlushIntent::Durable => budgets.durable_quiesce,
        FlushIntent::BestEffort => budgets.best_effort_quiesce,
    };
    if !gate.wait_quiesced(quiesce_budget) {
        if !was_paused {
            gate.resume();
        }
        return Err(FlushError::NotQuiesced(device));
    }

    let result = match intent {
        FlushIntent::Durable => sync(file).map_err(FlushError::Sync),
        FlushIntent::BestEffort => {
            let worker_file = match file.try_clone() {
                Ok(file) => file,
                Err(error) => {
                    if !was_paused {
                        gate.resume();
                    }
                    return Err(FlushError::Sync(error));
                }
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

    if !was_paused {
        gate.resume();
    }
    result
}

pub struct QuiesceReport {
    pub stuck: Vec<&'static str>,
}

impl QuiesceReport {
    pub fn is_empty(&self) -> bool {
        self.stuck.is_empty()
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
        loop {
            if dev.is_quiesced() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                stuck.push(dev.type_name());
                break;
            }
            thread::sleep(Duration::from_millis(5).min(deadline - now));
        }
    }
    QuiesceReport { stuck }
}

#[cfg(test)]
pub mod testing {
    use super::*;

    pub fn assert_query_non_blocking(dev: &Arc<dyn Lifecycle>) {
        let dev = Arc::clone(dev);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = dev.is_quiesced();
            tx.send(result).expect("is_quiesced result receiver");
        });
        rx.recv_timeout(Duration::from_millis(250))
            .expect("is_quiesced must not block");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct NeverQuiesces;

    struct FlushTestDevice {
        gate: QuiesceGate,
        file: File,
        sync_calls: Arc<AtomicUsize>,
        sync_delay: Duration,
        budgets: FlushBudgets,
    }

    impl FlushTestDevice {
        fn new(
            workers: usize,
            sync_delay: Duration,
            budgets: FlushBudgets,
        ) -> Self {
            Self {
                gate: QuiesceGate::new(workers),
                file: tempfile::tempfile().expect("create flush test file"),
                sync_calls: Arc::new(AtomicUsize::new(0)),
                sync_delay,
                budgets,
            }
        }
    }

    impl Lifecycle for FlushTestDevice {
        fn type_name(&self) -> &'static str {
            "flush-test"
        }

        fn flush_backing(&self, intent: FlushIntent) -> Result<(), FlushError> {
            let sync_calls = Arc::clone(&self.sync_calls);
            let sync_delay = self.sync_delay;
            flush_file_with(
                &self.gate,
                &self.file,
                intent,
                self.type_name(),
                self.budgets,
                move |_file| {
                    sync_calls.fetch_add(1, Ordering::AcqRel);
                    thread::sleep(sync_delay);
                    Ok(())
                },
            )
        }
    }

    const SHORT_BUDGETS: FlushBudgets = FlushBudgets {
        durable_quiesce: Duration::from_millis(20),
        best_effort_quiesce: Duration::from_millis(20),
        best_effort_sync: Duration::from_millis(20),
    };

    impl Lifecycle for NeverQuiesces {
        fn type_name(&self) -> &'static str {
            "never"
        }

        fn is_quiesced(&self) -> bool {
            false
        }
    }

    #[test]
    fn workers_park_and_resume() {
        let gate = Arc::new(QuiesceGate::new(2));
        assert!(!gate.is_quiesced());
        gate.pause();

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let gate = Arc::clone(&gate);
                thread::spawn(move || gate.park_if_paused())
            })
            .collect();

        assert!(gate.wait_quiesced(Duration::from_secs(1)));
        gate.resume();
        for handle in handles {
            handle.join().expect("quiesce worker");
        }
        assert_eq!(gate.active.load(Ordering::Acquire), 2);
    }

    #[test]
    fn wait_quiesced_reports_running_and_parked_workers() {
        let gate = Arc::new(QuiesceGate::new(1));
        assert!(!gate.wait_quiesced(Duration::from_millis(10)));

        gate.pause();
        let worker_gate = Arc::clone(&gate);
        let handle = thread::spawn(move || worker_gate.park_if_paused());
        assert!(gate.wait_quiesced(Duration::from_secs(1)));

        gate.resume();
        handle.join().expect("quiesce worker");
    }

    #[test]
    fn repeated_pause_resume_has_no_lost_wakeup() {
        let gate = Arc::new(QuiesceGate::new(2));
        let stop = Arc::new(AtomicBool::new(false));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        gate.park_if_paused();
                        thread::yield_now();
                    }
                })
            })
            .collect();

        for _ in 0..1000 {
            gate.pause();
            assert!(gate.wait_quiesced(Duration::from_secs(1)));
            gate.resume();

            let deadline = Instant::now() + Duration::from_secs(1);
            while gate.active.load(Ordering::Acquire) != 2
                && Instant::now() < deadline
            {
                thread::yield_now();
            }
            assert_eq!(gate.active.load(Ordering::Acquire), 2);
        }

        stop.store(true, Ordering::Release);
        gate.resume();
        for handle in handles {
            handle.join().expect("quiesce worker");
        }
    }

    #[test]
    fn wait_all_quiesced_honors_shared_deadline() {
        let devices: Vec<Arc<dyn Lifecycle>> = vec![Arc::new(NeverQuiesces)];
        let budget = Duration::from_millis(50);
        let started = Instant::now();

        let report = wait_all_quiesced(&devices, budget);

        assert_eq!(report.stuck, vec!["never"]);
        assert!(started.elapsed() <= budget * 2);
    }

    #[test]
    fn best_effort_never_syncs_a_backend_that_did_not_quiesce() {
        let device = FlushTestDevice::new(1, Duration::ZERO, SHORT_BUDGETS);

        let result = device.flush_backing(FlushIntent::BestEffort);

        assert!(matches!(result, Err(FlushError::NotQuiesced("flush-test"))));
        assert_eq!(device.sync_calls.load(Ordering::Acquire), 0);
        assert!(!device.gate.is_paused());
    }

    #[test]
    fn durable_never_syncs_a_backend_that_did_not_quiesce() {
        let device = FlushTestDevice::new(1, Duration::ZERO, SHORT_BUDGETS);

        let result = device.flush_backing(FlushIntent::Durable);

        assert!(matches!(result, Err(FlushError::NotQuiesced("flush-test"))));
        assert_eq!(device.sync_calls.load(Ordering::Acquire), 0);
        assert!(!device.gate.is_paused());
    }

    #[test]
    fn best_effort_sync_has_a_bounded_watchdog() {
        let budgets = FlushBudgets {
            best_effort_sync: BEST_EFFORT_SYNC_BUDGET,
            ..SHORT_BUDGETS
        };
        let device = FlushTestDevice::new(0, Duration::from_secs(30), budgets);
        let started = Instant::now();

        let result = device.flush_backing(FlushIntent::BestEffort);

        assert!(matches!(result, Err(FlushError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(6));
        assert!(!device.gate.is_paused());
    }

    #[test]
    fn durable_flush_syncs_a_real_file() {
        let gate = QuiesceGate::new(0);
        let mut file = tempfile::tempfile().expect("create durable flush file");
        file.write_all(b"durable data")
            .expect("write durable flush file");

        let result =
            flush_file(&gate, &file, FlushIntent::Durable, "real-file");

        assert!(result.is_ok());
    }

    #[test]
    fn flush_restores_the_entry_pause_state() {
        let running = FlushTestDevice::new(0, Duration::ZERO, SHORT_BUDGETS);
        running
            .flush_backing(FlushIntent::Durable)
            .expect("flush running device");
        assert!(!running.gate.is_paused());

        let paused = FlushTestDevice::new(0, Duration::ZERO, SHORT_BUDGETS);
        paused.gate.pause();
        paused
            .flush_backing(FlushIntent::Durable)
            .expect("flush paused device");
        assert!(paused.gate.is_paused());
    }
}
