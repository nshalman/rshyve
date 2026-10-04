// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use std::io::Write;

struct NeverQuiesces;

/// A drain is owned by whoever holds the scope. For a device reset
/// that is the guest, which can repeat it at will, so ending one
/// must not release the pause the control plane took to migrate.
#[test]
fn a_drain_scope_keeps_a_pause_taken_inside_it() {
    let gate = QuiesceGate::new(0);
    let drain = gate.drain_scope();
    assert!(gate.is_paused(), "a drain must hold the workers");

    gate.pause();
    drop(drain);

    assert!(gate.is_paused(), "the drain released the operator's pause");
    gate.resume();
    assert!(!gate.is_paused());
}

/// Two drains can overlap, a guest reset inside a device flush for
/// instance, and the first to end must not free the second.
#[test]
fn one_drain_scope_does_not_end_another() {
    let gate = QuiesceGate::new(0);
    let first = gate.drain_scope();
    let second = gate.drain_scope();

    drop(first);
    assert!(gate.is_paused(), "one drain ended another");

    drop(second);
    assert!(!gate.is_paused(), "the last drain left the workers parked");
}

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
            || true,
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

/// A producer that passed the pause check hands its work on before
/// the pause can start. Otherwise the workers park first, and the
/// work sits in a queue nobody serves while the pause reports quiet.
#[test]
fn a_pause_waits_for_an_admitted_producer() {
    let gate = Arc::new(QuiesceGate::new(0));
    let admission = gate.admit().expect("an idle gate admits");

    let (tx, rx) = mpsc::channel();
    let pauser = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            gate.pause();
            tx.send(()).expect("report the pause");
        })
    };
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "the pause started under an admitted producer"
    );

    drop(admission);
    rx.recv_timeout(Duration::from_secs(5))
        .expect("the pause did not start once the producer left");
    pauser.join().expect("pause thread");
    assert!(gate.admit().is_none(), "a paused gate admitted work");

    // A drain closes admission the same way.
    gate.resume();
    let drain = gate.drain_scope();
    assert!(gate.admit().is_none(), "a drain admitted work");
    drop(drain);
    assert!(gate.admit().is_some());
}

/// Work handed to a worker that is still parked has to run, or a
/// pause waits for it until its deadline.
#[test]
fn a_parked_worker_takes_work_it_finds() {
    let gate = Arc::new(QuiesceGate::new(1));
    gate.pause();
    let (work_tx, work_rx) = mpsc::channel::<u32>();
    let worker = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            gate.park_if_paused_or(|| work_rx.try_recv().ok())
        })
    };
    assert!(gate.wait_quiesced(Duration::from_secs(5)));

    work_tx.send(7).expect("hand over work");
    assert_eq!(worker.join().expect("worker"), Some(7));
    assert!(gate.is_paused(), "taking work ended the pause");
    assert_eq!(gate.active.load(Ordering::Acquire), 1);
}

/// A pause or a drain is in force before it waits for the producers
/// already admitted, so a producer that comes later backs out at once.
#[test]
fn a_waiting_pause_turns_new_producers_away() {
    for drain in [false, true] {
        let gate = Arc::new(QuiesceGate::new(0));
        let admission = gate.admit().expect("an idle gate admits");
        let holder = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                if drain {
                    drop(gate.drain_scope());
                } else {
                    gate.pause();
                }
            })
        };
        wait_until(|| gate.is_paused());

        let (tx, rx) = mpsc::channel();
        let late = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                tx.send(gate.admit().is_none()).expect("report the admit");
            })
        };
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "a producer got past a waiting {}",
            if drain { "drain" } else { "pause" }
        );

        drop(admission);
        holder.join().expect("pause thread");
        late.join().expect("producer thread");
    }
}

/// A worker part way through a request: it moves on past the hold,
/// says so, and parks as it would once the request is done.
fn in_transfer(
    gate: &Arc<QuiesceGate>,
) -> (mpsc::Receiver<()>, thread::JoinHandle<()>) {
    let gate = Arc::clone(gate);
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        gate.hold_in_transfer();
        tx.send(()).expect("report the transfer");
        gate.park_if_paused();
    });
    (rx, worker)
}

/// A request part way through runs on while a pause is young, and
/// stops once the pause has lasted the grace. Held, it still counts
/// as active, so the gate does not report quiet.
#[test]
fn a_transfer_is_held_only_after_the_grace() {
    let gate = Arc::new(QuiesceGate::new(1));
    gate.pause_with_grace(Duration::from_secs(60));
    let (moved, worker) = in_transfer(&gate);
    moved
        .recv_timeout(Duration::from_secs(5))
        .expect("a pause inside its grace held the transfer");
    assert!(gate.wait_quiesced(Duration::from_secs(5)));
    gate.resume();
    worker.join().expect("worker");

    gate.pause_with_grace(Duration::ZERO);
    let (moved, worker) = in_transfer(&gate);
    // Let through, the worker would park and the gate read quiet.
    assert!(
        !gate.wait_quiesced(Duration::from_millis(200)),
        "a pause past its grace let the transfer run"
    );
    assert!(moved.try_recv().is_err());
    gate.resume();
    moved
        .recv_timeout(Duration::from_secs(5))
        .expect("the resume did not free the transfer");
    worker.join().expect("worker");

    // A drain wants the transfer finished, so it never holds one.
    let drain = gate.drain_scope();
    let (moved, worker) = in_transfer(&gate);
    moved
        .recv_timeout(Duration::from_secs(5))
        .expect("a drain held the transfer");
    assert!(gate.wait_quiesced(Duration::from_secs(5)));
    drop(drain);
    worker.join().expect("worker");
}

/// A retry after a resume gets its own grace, not what is left of the
/// last pause's.
#[test]
fn a_pause_after_a_resume_starts_a_fresh_grace() {
    let gate = Arc::new(QuiesceGate::new(1));
    gate.pause_with_grace(Duration::ZERO);
    gate.resume();

    gate.pause_with_grace(Duration::from_secs(60));
    let (moved, worker) = in_transfer(&gate);
    moved
        .recv_timeout(Duration::from_secs(5))
        .expect("the retry was held on the last pause's grace");
    gate.resume();
    worker.join().expect("worker");
}

#[test]
fn a_second_pause_keeps_the_first_start_and_grace() {
    let gate = QuiesceGate::new(0);
    gate.pause_with_grace(Duration::from_secs(60));
    let first = gate.state().paused_at;
    thread::sleep(Duration::from_millis(5));
    gate.pause_with_grace(Duration::ZERO);
    {
        let s = gate.state();
        assert_eq!(s.paused_at, first);
        assert_eq!(s.grace, Duration::from_secs(60));
    }
    gate.resume();
    assert!(gate.state().paused_at.is_none());
}

#[test]
fn a_flush_waits_for_work_the_gate_does_not_count() {
    let device = FlushTestDevice::new(0, Duration::ZERO, SHORT_BUDGETS);
    let result = flush_file_with(
        &device.gate,
        &device.file,
        FlushIntent::Durable,
        "flush-test",
        SHORT_BUDGETS,
        || false,
        |_file| panic!("synced with work in flight"),
    );
    assert!(matches!(result, Err(FlushError::NotQuiesced("flush-test"))));
}

/// Parked workers with work still out must not make the wait spin: it
/// holds the lock a worker that takes that work needs.
#[test]
fn a_wait_for_idle_polls_and_does_not_spin() {
    let gate = QuiesceGate::new(0);
    let polls = AtomicUsize::new(0);
    let budget = Duration::from_millis(100);

    assert!(!gate.wait_quiesced_and(budget, || {
        polls.fetch_add(1, Ordering::Relaxed);
        false
    }));

    // A sleep never ends early, so this bound holds on any machine.
    let most = (budget.as_millis() / IDLE_POLL.as_millis()) as usize + 2;
    let polls = polls.load(Ordering::Relaxed);
    assert!(polls <= most, "{polls} polls in {budget:?}");
}

fn wait_until(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(1));
    }
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

    let report = wait_all_quiesced_named(&devices, Duration::from_millis(20));

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
        device: Arc::new(FlushTestDevice::new(0, Duration::ZERO, SHORT_BUDGETS))
            as Arc<dyn Lifecycle>,
    }];

    let report = wait_all_quiesced_named(&devices, Duration::from_millis(20));

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

    let result = flush_file(&gate, &file, FlushIntent::Durable, "real-file");

    assert!(result.is_ok());
}

#[test]
fn sync_file_runs_without_a_gate() {
    let mut file = tempfile::tempfile().expect("create sync file");
    file.write_all(b"drained by the device")
        .expect("write sync file");

    sync_file(&file, FlushIntent::Durable, "self-drained")
        .expect("durable sync");
    sync_file(&file, FlushIntent::BestEffort, "self-drained")
        .expect("best-effort sync");
}

#[test]
fn sync_file_keeps_the_best_effort_watchdog() {
    let file = tempfile::tempfile().expect("create sync file");
    let started = Instant::now();

    let result = sync_file_with(
        &file,
        FlushIntent::BestEffort,
        "self-drained",
        SHORT_BUDGETS,
        |_file| {
            thread::sleep(Duration::from_secs(5));
            Ok(())
        },
    );

    assert!(matches!(result, Err(FlushError::TimedOut)));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn quiesce_budget_follows_the_intent() {
    assert_eq!(quiesce_budget(FlushIntent::Durable), DURABLE_QUIESCE_BUDGET);
    assert_eq!(
        quiesce_budget(FlushIntent::BestEffort),
        BEST_EFFORT_QUIESCE_BUDGET
    );
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
