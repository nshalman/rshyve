// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest exit handling, device quiesce, and VM teardown.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use slog::{error, info, warn, Logger};
use vmm_core::exits::Suspend;
use vmm_core::machine::Machine;
use vmm_devices::quiesce::{wait_all_quiesced_named, NamedDevice};

use crate::registry::DeviceRegistry;
use crate::vcpu_tasks::VcpuEvent;

const QUIESCE_BUDGET: Duration = Duration::from_secs(5);
const VCPU_JOIN_BUDGET: Duration = Duration::from_secs(5);

/// How often the event loop rechecks a roster that can grow.
///
/// A fixed vCPU set is finished when the event channel closes, because
/// the threads hold the only senders. A roster keeps one more sender
/// for the CPU that is not added yet, so the channel never closes and
/// the loop asks the roster whether any thread is left instead.
const ROSTER_POLL: Duration = Duration::from_millis(250);

const ROSTER_POISONED: &str = "vCPU roster lock poisoned";

/// Why the guest stopped running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Reboot,
    PowerOff,
    Halt,
    TripleFault(i32),
    /// The guest triggered a VM exit that userspace cannot emulate.
    GuestFault,
}

/// Pause every device, wait for the backends to go quiet, then flush.
pub fn quiesce_devices(
    devices: &[Arc<dyn vmm_devices::Lifecycle>],
    log: &Logger,
) -> vmm_devices::QuiesceReport {
    let named = devices
        .iter()
        .map(|device| NamedDevice {
            id: None,
            device: device.clone(),
        })
        .collect::<Vec<_>>();
    quiesce_named(&named, log)
}

/// The same teardown, driven from the run-time registry.
///
/// The registry names each device, so a stuck one is reported by id and
/// not by a type name two devices share.
pub fn quiesce_registered_devices(
    registry: &DeviceRegistry,
    log: &Logger,
) -> vmm_devices::QuiesceReport {
    quiesce_named(&registry_named_devices(registry), log)
}

/// Snapshot the registry as named lifecycle handles.
///
/// The handles are cloned out, so no registry lock is held while a
/// device runs. Public because the control path pauses the same set and
/// must name a stuck device the same way.
pub fn registry_named_devices(registry: &DeviceRegistry) -> Vec<NamedDevice> {
    registry
        .list()
        .into_iter()
        .filter_map(|slot| {
            slot.lifecycle.map(|device| NamedDevice {
                id: Some(slot.id),
                device,
            })
        })
        .collect()
}

fn quiesce_named(
    devices: &[NamedDevice],
    log: &Logger,
) -> vmm_devices::QuiesceReport {
    for named in devices {
        // Teardown may follow an operator pause. Keep Indicator's transition
        // validation strict while making repeated quiescence idempotent.
        if named.device.lifecycle_state()
            != Some(vmm_devices::IndicatedState::Pause)
        {
            named.device.pause();
        }
    }

    let report = wait_all_quiesced_named(devices, QUIESCE_BUDGET);
    if !report.is_empty() {
        warn!(log, "devices did not quiesce before teardown";
            "devices" => ?report.names());
    }

    for named in devices {
        if let Err(error) = named
            .device
            .flush_backing(vmm_devices::FlushIntent::BestEffort)
        {
            warn!(log, "device flush incomplete at teardown";
                "device" => named.device.type_name(),
                "id" => named.id.as_deref(),
                "error" => ?error);
        }
    }

    report
}

/// The vCPU threads that are running, which can grow at run time.
///
/// The boot set is fixed, but a CPU brought online while the guest
/// runs adds a thread, so the join at teardown cannot work from a
/// snapshot taken at boot.
///
/// There is no matching remove. illumos has no `vm_deactivate_cpu`:
/// `active_cpus` is only ever set, and only `vm_init` clears it,
/// wholesale, on `VM_REINIT` (`vmm.c:567`). CPU hot-REMOVE is
/// therefore not possible at the kernel boundary, and this type does
/// not pretend otherwise.
pub struct VcpuThreads {
    inner: Mutex<Roster>,
}

struct Roster {
    handles: Vec<JoinHandle<()>>,
    /// Teardown has taken the handles. A later add must be refused, or
    /// its thread would never be joined.
    closed: bool,
}

impl VcpuThreads {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Roster {
                handles: Vec::new(),
                closed: false,
            }),
        })
    }

    /// Record a thread to join at teardown.
    ///
    /// After [`close`](Self::close) the handle comes back instead, so
    /// the caller can undo the spawn rather than leak the thread.
    pub fn push(&self, handle: JoinHandle<()>) -> Result<(), JoinHandle<()>> {
        let mut roster = self.inner.lock().expect(ROSTER_POISONED);
        if roster.closed {
            return Err(handle);
        }
        // A rolled-back add leaves a finished handle behind. Dropping
        // it here keeps a long-running VM from collecting one per
        // refused add.
        roster.handles.retain(|handle| !handle.is_finished());
        roster.handles.push(handle);
        Ok(())
    }

    /// Recorded threads that have not finished.
    pub fn live(&self) -> usize {
        let roster = self.inner.lock().expect(ROSTER_POISONED);
        roster.handles.iter().filter(|h| !h.is_finished()).count()
    }

    /// Recorded threads, finished ones included.
    pub fn len(&self) -> usize {
        self.inner.lock().expect(ROSTER_POISONED).handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().expect(ROSTER_POISONED).closed
    }

    /// Take every handle and refuse later pushes.
    pub fn close(&self) -> Vec<JoinHandle<()>> {
        let mut roster = self.inner.lock().expect(ROSTER_POISONED);
        roster.closed = true;
        std::mem::take(&mut roster.handles)
    }
}

/// The vCPU threads an event loop waits on.
pub enum VcpuSet<'a> {
    /// A set made at boot. The event channel closing means every
    /// thread has gone.
    Fixed(Vec<JoinHandle<()>>),
    /// A set the control plane can add to while the guest runs.
    Roster(&'a Arc<VcpuThreads>),
}

impl VcpuSet<'_> {
    /// Wait for the next vCPU event, or `None` when no vCPU thread is
    /// left to send one.
    fn next_event(
        &self,
        event_rx: &mpsc::Receiver<VcpuEvent>,
    ) -> Option<VcpuEvent> {
        match self {
            VcpuSet::Fixed(_) => event_rx.recv().ok(),
            VcpuSet::Roster(threads) => loop {
                match event_rx.recv_timeout(ROSTER_POLL) {
                    Ok(event) => return Some(event),
                    Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                    // Every exit path of a vCPU run loop sends an
                    // event first, so this only catches a thread that
                    // died without one, a panic for instance.
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if threads.live() == 0 {
                            return None;
                        }
                    }
                }
            },
        }
    }

    fn join(self, budget: Duration) -> bool {
        match self {
            VcpuSet::Fixed(handles) => join_vcpus_bounded(handles, budget),
            VcpuSet::Roster(threads) => {
                join_vcpus_bounded(threads.close(), budget)
            }
        }
    }
}

/// Join every vCPU thread, giving up after `budget`.
///
/// Returns false when the budget expired. A wedged vCPU thread must not
/// hold the bhyve instance open, so the caller destroys the VM anyway.
pub fn join_vcpus_bounded(
    handles: Vec<JoinHandle<()>>,
    budget: Duration,
) -> bool {
    let (tx, rx) = mpsc::channel();
    let worker =
        thread::Builder::new()
            .name("vcpu-join".into())
            .spawn(move || {
                for handle in handles {
                    drop(handle.join());
                }
                let _ = tx.send(());
            });
    if worker.is_err() {
        return false;
    }

    matches!(rx.recv_timeout(budget), Ok(()))
}

/// Where the teardown path finds the devices to quiesce.
enum DeviceSource<'a> {
    Snapshot(&'a [Arc<dyn vmm_devices::Lifecycle>]),
    /// Read after the guest stops, so a device that was hot-removed
    /// during the run is not paused after its eject.
    Registry(&'a DeviceRegistry),
}

impl DeviceSource<'_> {
    fn quiesce(&self, log: &Logger) -> vmm_devices::QuiesceReport {
        match self {
            DeviceSource::Snapshot(devices) => quiesce_devices(devices, log),
            DeviceSource::Registry(registry) => {
                quiesce_registered_devices(registry, log)
            }
        }
    }
}

/// Wait for the first vCPU event, quiesce the devices, join the vCPU
/// threads, clean up the control socket, and destroy the VM.
pub fn run_event_loop(
    machine: &Machine,
    event_rx: mpsc::Receiver<VcpuEvent>,
    handles: Vec<JoinHandle<()>>,
    control_cleanup: Option<PathBuf>,
    devices: &[Arc<dyn vmm_devices::Lifecycle>],
    log: &Logger,
) -> anyhow::Result<RunOutcome> {
    run_event_loop_with(
        machine,
        event_rx,
        VcpuSet::Fixed(handles),
        control_cleanup,
        DeviceSource::Snapshot(devices),
        log,
    )
}

/// The same loop, quiescing whatever the registry holds at exit.
pub fn run_event_loop_registered(
    machine: &Machine,
    event_rx: mpsc::Receiver<VcpuEvent>,
    handles: Vec<JoinHandle<()>>,
    control_cleanup: Option<PathBuf>,
    registry: &DeviceRegistry,
    log: &Logger,
) -> anyhow::Result<RunOutcome> {
    run_event_loop_with(
        machine,
        event_rx,
        VcpuSet::Fixed(handles),
        control_cleanup,
        DeviceSource::Registry(registry),
        log,
    )
}

/// The same loop again, over a vCPU set that can grow while the guest
/// runs.
pub fn run_event_loop_growable(
    machine: &Machine,
    event_rx: mpsc::Receiver<VcpuEvent>,
    threads: &Arc<VcpuThreads>,
    control_cleanup: Option<PathBuf>,
    registry: &DeviceRegistry,
    log: &Logger,
) -> anyhow::Result<RunOutcome> {
    run_event_loop_with(
        machine,
        event_rx,
        VcpuSet::Roster(threads),
        control_cleanup,
        DeviceSource::Registry(registry),
        log,
    )
}

fn run_event_loop_with(
    machine: &Machine,
    event_rx: mpsc::Receiver<VcpuEvent>,
    vcpus: VcpuSet<'_>,
    control_cleanup: Option<PathBuf>,
    devices: DeviceSource<'_>,
    log: &Logger,
) -> anyhow::Result<RunOutcome> {
    let outcome = outcome_of(vcpus.next_event(&event_rx), log);

    // A wedged backend must not prevent release of the bhyve instance.
    let _report = devices.quiesce(log);

    if !vcpus.join(VCPU_JOIN_BUDGET) {
        warn!(log, "vCPU threads did not exit; destroying VM anyway");
    }

    // The socket path may already be gone.
    if let Some(ref path) = control_cleanup {
        let _ = std::fs::remove_file(path);
    }

    // WHY no destroy in the crash paths: the machine builder arms
    // VM_SET_AUTODESTRUCT, so the kernel reclaims the instance when the fd
    // closes. This explicit destroy stays the deterministic commit point,
    // ordered after quiesce and vCPU join.
    if let Err(e) = machine.hdl().destroy() {
        warn!(log, "failed to destroy VM"; "error" => %e);
    }

    info!(log, "VM shutdown complete");
    outcome
}

fn outcome_of(
    event: Option<VcpuEvent>,
    log: &Logger,
) -> anyhow::Result<RunOutcome> {
    match event {
        Some(VcpuEvent::Suspended { vcpu_id, kind }) => {
            info!(log, "VM suspended";
                "vcpu" => vcpu_id,
                "kind" => ?kind,
            );
            Ok(match kind {
                Suspend::Reset => RunOutcome::Reboot,
                Suspend::PowerOff => RunOutcome::PowerOff,
                Suspend::Halt => RunOutcome::Halt,
                Suspend::TripleFault(src) => RunOutcome::TripleFault(src),
            })
        }
        Some(VcpuEvent::Error {
            vcpu_id,
            error: err,
        }) => {
            error!(log, "vCPU error";
                "vcpu" => vcpu_id,
                "error" => &err,
            );
            Ok(RunOutcome::GuestFault)
        }
        None => {
            warn!(log, "all vCPU threads exited without events");
            Ok(RunOutcome::Halt)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use slog::Logger;
    use vmm_devices::Lifecycle as _;

    use super::{
        join_vcpus_bounded, quiesce_devices, RunOutcome, VcpuSet, VcpuThreads,
        QUIESCE_BUDGET, ROSTER_POLL,
    };
    use crate::vcpu_tasks::VcpuEvent;
    use std::sync::mpsc;

    struct TrackedDevice {
        indicator: vmm_devices::Indicator,
    }

    impl vmm_devices::Lifecycle for TrackedDevice {
        fn type_name(&self) -> &'static str {
            "tracked-test-device"
        }

        fn lifecycle_state(&self) -> Option<vmm_devices::IndicatedState> {
            Some(self.indicator.state())
        }

        fn start(&self) -> anyhow::Result<()> {
            self.indicator.start();
            Ok(())
        }

        fn pause(&self) {
            self.indicator.pause();
        }
    }

    struct NeverQuiesces;

    impl vmm_devices::Lifecycle for NeverQuiesces {
        fn type_name(&self) -> &'static str {
            "never"
        }

        fn is_quiesced(&self) -> bool {
            false
        }

        fn flush_backing(
            &self,
            _intent: vmm_devices::FlushIntent,
        ) -> Result<(), vmm_devices::FlushError> {
            Err(vmm_devices::FlushError::NotQuiesced("never"))
        }
    }

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn quiesce_skips_device_that_is_already_paused() {
        // Indicator panics on a Pause to Pause transition, so teardown
        // after an operator pause must not pause again.
        let device = Arc::new(TrackedDevice {
            indicator: vmm_devices::Indicator::new(),
        });
        device.start().expect("test device should start");
        device.pause();
        let devices: Vec<Arc<dyn vmm_devices::Lifecycle>> =
            vec![device.clone()];

        let report = quiesce_devices(&devices, &null_log());

        assert!(report.is_empty());
        assert_eq!(
            device.lifecycle_state(),
            Some(vmm_devices::IndicatedState::Pause),
        );
    }

    #[test]
    fn quiesce_reports_device_that_misses_deadline() {
        let devices: Vec<Arc<dyn vmm_devices::Lifecycle>> =
            vec![Arc::new(NeverQuiesces)];
        let started = Instant::now();

        let report = quiesce_devices(&devices, &null_log());

        assert_eq!(report.stuck, vec!["never"]);
        assert!(started.elapsed() <= QUIESCE_BUDGET * 2);
    }

    #[test]
    fn join_vcpus_bounded_returns_true_with_no_threads() {
        assert!(join_vcpus_bounded(Vec::new(), Duration::from_secs(1)));
    }

    #[test]
    fn vcpu_join_timeout_is_bounded() {
        // A wedged vCPU thread must not hold the bhyve instance open.
        let budget = Duration::from_millis(20);
        let handle = thread::spawn(move || thread::sleep(budget * 5));
        let started = Instant::now();

        let joined = join_vcpus_bounded(vec![handle], budget);

        assert!(!joined);
        assert!(started.elapsed() <= budget * 2);
    }

    #[test]
    fn run_outcome_distinguishes_triple_fault_sources() {
        assert_ne!(RunOutcome::TripleFault(0), RunOutcome::TripleFault(1));
        assert_eq!(RunOutcome::Halt, RunOutcome::Halt);
    }

    /// A thread that runs until its channel is dropped, standing in
    /// for a vCPU thread inside the guest.
    fn parked_thread() -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel::<()>();
        // The recv fails when the sender is dropped, which is how
        // the test stops the stand-in thread.
        let handle = thread::spawn(move || if rx.recv().is_ok() {});
        (tx, handle)
    }

    /// Wait for a roster to report `want` live threads, or give up.
    fn wait_for_live(threads: &VcpuThreads, want: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if threads.live() == want {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn a_roster_grows_while_the_guest_runs() {
        // The boot set is fixed, but a CPU added later has a thread
        // that teardown still has to join.
        let threads = VcpuThreads::new();
        let (boot, boot_handle) = parked_thread();
        threads
            .push(boot_handle)
            .expect("a fresh roster accepts a thread");
        assert_eq!(threads.len(), 1);

        let (late, late_handle) = parked_thread();
        threads
            .push(late_handle)
            .expect("an open roster accepts a thread");
        assert_eq!(threads.len(), 2);

        drop(boot);
        drop(late);
        assert!(join_vcpus_bounded(threads.close(), Duration::from_secs(5)));
    }

    #[test]
    fn a_closed_roster_hands_the_handle_back() {
        // A thread pushed after teardown took the handles would never
        // be joined, so the add has to be refused and undone.
        let threads = VcpuThreads::new();
        assert!(threads.close().is_empty());
        assert!(threads.is_closed());

        let (park, handle) = parked_thread();
        let refused =
            threads.push(handle).expect_err("a closed roster refuses");
        drop(park);
        refused.join().expect("the refused thread should not panic");
        assert!(threads.is_empty());
    }

    #[test]
    fn a_roster_drops_the_threads_a_failed_add_left_behind() {
        // A rolled-back add leaves a finished handle. Without the
        // prune, a long-running VM collects one per refused add.
        let threads = VcpuThreads::new();
        let (park, handle) = parked_thread();
        drop(park);
        threads
            .push(handle)
            .expect("a fresh roster accepts a thread");
        assert!(wait_for_live(&threads, 0));

        let (still_parked, live_handle) = parked_thread();
        threads
            .push(live_handle)
            .expect("an open roster accepts a thread");

        assert_eq!(threads.len(), 1, "the finished handle was dropped");
        drop(still_parked);
        assert!(join_vcpus_bounded(threads.close(), Duration::from_secs(5)));
    }

    #[test]
    fn a_fixed_set_stops_when_the_event_channel_closes() {
        let (tx, rx) = mpsc::channel::<VcpuEvent>();
        drop(tx);

        assert!(VcpuSet::Fixed(Vec::new()).next_event(&rx).is_none());
    }

    #[test]
    fn a_roster_stops_when_no_thread_is_left() {
        // The registry holds a sender for the CPU that is not added
        // yet, so the channel never closes. The roster is what says
        // that every vCPU thread has gone.
        let threads = VcpuThreads::new();
        let (park, handle) = parked_thread();
        threads
            .push(handle)
            .expect("a fresh roster accepts a thread");
        let (_registry_sender, rx) = mpsc::channel::<VcpuEvent>();

        drop(park);
        assert!(wait_for_live(&threads, 0));
        let started = Instant::now();
        let event = VcpuSet::Roster(&threads).next_event(&rx);

        assert!(event.is_none());
        assert!(started.elapsed() <= ROSTER_POLL * 8);
    }

    #[test]
    fn a_roster_returns_the_event_a_late_thread_sends() {
        let threads = VcpuThreads::new();
        let (park, handle) = parked_thread();
        threads
            .push(handle)
            .expect("a fresh roster accepts a thread");
        let (sender, rx) = mpsc::channel::<VcpuEvent>();

        sender
            .send(VcpuEvent::Error {
                vcpu_id: 3,
                error: "late vCPU".into(),
            })
            .expect("the test holds the receiver");
        let event = VcpuSet::Roster(&threads).next_event(&rx);

        assert!(matches!(event, Some(VcpuEvent::Error { vcpu_id: 3, .. })));
        drop(park);
        assert!(join_vcpus_bounded(threads.close(), Duration::from_secs(5)));
    }
}
