// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest exit handling, device quiesce, and VM teardown.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use slog::{error, info, warn, Logger};
use vmm_core::exits::Suspend;
use vmm_core::hdl::VmmHdl;
use vmm_core::machine::Machine;
use vmm_devices::quiesce::{wait_all_quiesced_named, NamedDevice};

use crate::registry::DeviceRegistry;
use crate::vcpu_tasks::VcpuEvent;

const QUIESCE_BUDGET: Duration = Duration::from_secs(5);
const VCPU_JOIN_BUDGET: Duration = Duration::from_secs(5);

/// How long one device gets to flush its backing store.
///
/// `flush_backing` is one synchronous call into a backend, so no budget
/// here can interrupt it. What the count buys is the size of the clock
/// the terminal watchdog is given, so a VM with many disks is not ended
/// while it is still writing.
const FLUSH_BUDGET_PER_DEVICE: Duration = Duration::from_secs(2);

/// How long `VM_DESTROY_SELF` gets before teardown stops waiting on it.
///
/// WHY teardown cannot simply call it: on illumos the ioctl purges the
/// vmm_drv holds and waits for every lease to break in an untimed
/// `cv_wait` (`vmm_sol_dev.c`, `vmm_drv_purge`). A lease that never
/// breaks holds the calling thread for as long as it is held, and no
/// signal, close or exit reaches a thread parked there.
const DESTROY_BUDGET: Duration = Duration::from_secs(15);

/// The least teardown gets, whatever the device count.
///
/// The escalation this deadline replaces ended a VMM 50 s after SIGTERM
/// without looking at teardown at all. A smaller budget would make the
/// terminal exit more eager than the one it replaces, and would cut off
/// a slow but working flush.
const TEARDOWN_BUDGET_MIN: Duration = Duration::from_secs(60);

/// The most teardown may claim from the terminal watchdog.
///
/// A device count large enough to overflow the sum must not disarm the
/// watchdog by asking for a deadline it can never reach.
const TEARDOWN_BUDGET_MAX: Duration = Duration::from_secs(300);

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
    vmm_core::thread::join_bounded(handles, budget)
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

    /// The most devices the flush can have to walk.
    fn len(&self) -> usize {
        match self {
            DeviceSource::Snapshot(devices) => devices.len(),
            DeviceSource::Registry(registry) => registry.len(),
        }
    }
}

/// The clock the terminal watchdog holds this teardown to.
///
/// Quiesce, the vCPU join and the destroy cannot run past their own
/// budgets. The flush can: it is one synchronous backend call per
/// device, so a VM with many disks needs more time than a VM with one,
/// and no constant can know which it is.
fn teardown_budget(devices: usize) -> Duration {
    let flush = u32::try_from(devices)
        .ok()
        .and_then(|count| FLUSH_BUDGET_PER_DEVICE.checked_mul(count))
        .unwrap_or(TEARDOWN_BUDGET_MAX);
    QUIESCE_BUDGET
        .saturating_add(flush)
        .saturating_add(VCPU_JOIN_BUDGET)
        .saturating_add(DESTROY_BUDGET)
        .clamp(TEARDOWN_BUDGET_MIN, TEARDOWN_BUDGET_MAX)
}

/// Destroy the VM, and stop waiting on the call after `budget`.
///
/// The ioctl is not bounded by anything userspace holds, so it runs on
/// its own thread and the rest of teardown does not wait past the
/// budget. A thread that misses the budget keeps its handle, because
/// the kernel is still using the file descriptor behind it.
///
/// Returns false when the destroy did not return. The instance is then
/// left to `VM_SET_AUTODESTRUCT`, which the machine builder armed.
fn destroy_bounded(hdl: &Arc<VmmHdl>, budget: Duration, log: &Logger) -> bool {
    let hdl = hdl.clone();
    destroy_bounded_with(move || hdl.destroy(), budget, log)
}

/// The same bound over a caller-supplied destroy.
///
/// Split out so a destroy that never returns is testable without a live
/// /dev/vmm handle.
fn destroy_bounded_with(
    destroy: impl FnOnce() -> io::Result<()> + Send + 'static,
    budget: Duration,
    log: &Logger,
) -> bool {
    let (tx, rx) = mpsc::channel();
    let spawned =
        thread::Builder::new()
            .name("vm-destroy".into())
            .spawn(move || {
                // A closed channel means teardown stopped waiting, which is
                // the wedge this bound exists for. The send result says
                // nothing the receiver does not already know.
                drop(tx.send(destroy()));
            });
    if let Err(e) = spawned {
        // A destroy on this thread would have no bound at all.
        error!(log, "could not start the VM destroy; leaving it to autodestruct";
            "error" => %e);
        return false;
    }

    match rx.recv_timeout(budget) {
        Ok(Ok(())) => true,
        // The call returned, so teardown is over even though it failed.
        Ok(Err(e)) => {
            warn!(log, "failed to destroy VM"; "error" => %e);
            true
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            error!(log, "the VM destroy did not return; a vmm_drv lease is still held";
                "budget_secs" => budget.as_secs());
            false
        }
        // The thread went without sending, so nothing says the ioctl
        // ran. Report it the same way: the instance is not known gone.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            error!(log, "the VM destroy thread ended with no result");
            false
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
    // The device flush below is one synchronous backend call per
    // device, and the destroy after it is unbounded in the kernel.
    // Neither can be interrupted from here, so the terminal watchdog
    // keeps a clock across the whole teardown and is given a budget
    // that scales with the work.
    crate::signal::arm_teardown_deadline(teardown_budget(devices.len()));

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
    if destroy_bounded(machine.hdl(), DESTROY_BUDGET, log) {
        info!(log, "VM shutdown complete");
    }
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
        destroy_bounded_with, join_vcpus_bounded, quiesce_devices,
        teardown_budget, RunOutcome, VcpuSet, VcpuThreads,
        FLUSH_BUDGET_PER_DEVICE, QUIESCE_BUDGET, ROSTER_POLL,
        TEARDOWN_BUDGET_MAX, TEARDOWN_BUDGET_MIN,
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
        // Teardown after an operator pause must not pause again: the
        // device is already quiesced and pausing it twice would hide a
        // real double pause behind a satisfied request.
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
    fn a_destroy_that_never_returns_does_not_hold_teardown_open() {
        // VM_DESTROY_SELF waits for every vmm_drv lease to break, in an
        // untimed cv_wait. Teardown must stop waiting on it, and must
        // not report the shutdown as complete.
        let (park, blocked) = mpsc::channel::<()>();
        let budget = Duration::from_millis(50);
        let started = Instant::now();

        let destroyed = destroy_bounded_with(
            move || {
                // The recv ends only when the test drops the sender,
                // which is how the stand-in call is released.
                blocked.recv().ok();
                Ok(())
            },
            budget,
            &null_log(),
        );

        assert!(!destroyed, "a destroy that never returned is not a success");
        assert!(started.elapsed() <= budget * 20);
        drop(park);
    }

    #[test]
    fn a_destroy_that_returns_is_reported_either_way() {
        // The call came back, so teardown is over. An error from the
        // kernel is logged, not waited on again.
        assert!(destroy_bounded_with(|| Ok(()), QUIESCE_BUDGET, &null_log()));
        assert!(destroy_bounded_with(
            || Err(std::io::Error::from(std::io::ErrorKind::InvalidInput)),
            QUIESCE_BUDGET,
            &null_log(),
        ));
    }

    #[test]
    fn the_teardown_budget_grows_with_the_device_count() {
        // The flush is one synchronous call per device, so the clock a
        // 40-disk VM needs is not the clock a 20-disk VM needs.
        let few = teardown_budget(20);
        let many = teardown_budget(40);

        assert!(few < many);
        assert_eq!(many - few, FLUSH_BUDGET_PER_DEVICE * 20);
    }

    #[test]
    fn a_teardown_budget_stays_inside_its_bounds() {
        // Under the floor the terminal exit would be more eager than
        // the fixed escalation it replaces. Over the ceiling the
        // deadline could never be reached, which disarms the watchdog
        // the budget exists to arm.
        assert_eq!(teardown_budget(0), TEARDOWN_BUDGET_MIN);
        assert_eq!(teardown_budget(usize::MAX), TEARDOWN_BUDGET_MAX);
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
