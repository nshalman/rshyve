// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt;
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

/// Shared pause/drain gate for devices with I/O worker threads.
///
/// WHY a bool predicate and not a future: no caller in this tree is async
/// (`bin/rshyve/src/reboot.rs`, `bin/rshyve/src/control.rs` both drive it from a
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
        if crate::blkdev::datasync(file.as_raw_fd()) == 0 {
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
    fn named_wait_tells_two_devices_of_one_type_apart() {
        // Two virtio-blk devices share a type name, so only the id can
        // say which one is wedged.
        let devices = vec![
            NamedDevice {
                id: Some("blk@4".to_string()),
                device: Arc::new(NeverQuiesces) as Arc<dyn Lifecycle>,
            },
            NamedDevice {
                id: Some("blk@5".to_string()),
                device: Arc::new(NeverQuiesces) as Arc<dyn Lifecycle>,
            },
        ];

        let report =
            wait_all_quiesced_named(&devices, Duration::from_millis(20));

        assert_eq!(report.stuck, vec!["never", "never"]);
        assert_eq!(report.names(), ["never (blk@4)", "never (blk@5)"]);
        assert_eq!(report.devices()[0].id.as_deref(), Some("blk@4"));
    }

    #[test]
    fn a_report_with_no_ids_names_the_type() {
        let report = QuiesceReport::new(vec!["virtio-blk"]);

        assert!(!report.is_empty());
        assert_eq!(report.stuck, vec!["virtio-blk"]);
        assert_eq!(report.names(), ["virtio-blk"]);
        assert!(QuiesceReport::new(Vec::new()).is_empty());
    }

    #[test]
    fn a_quiesced_device_is_not_reported() {
        let devices = vec![NamedDevice {
            id: Some("stub@4".to_string()),
            device: Arc::new(FlushTestDevice::new(
                0,
                Duration::ZERO,
                SHORT_BUDGETS,
            )) as Arc<dyn Lifecycle>,
        }];

        let report =
            wait_all_quiesced_named(&devices, Duration::from_millis(20));

        assert!(report.is_empty());
        assert!(report.devices().is_empty());
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
