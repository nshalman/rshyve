// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest exit handling, device quiesce, and VM teardown.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use slog::{error, info, warn, Logger};
use vmm_core::exits::Suspend;
use vmm_core::machine::Machine;

use crate::vcpu_tasks::VcpuEvent;

const QUIESCE_BUDGET: Duration = Duration::from_secs(5);
const VCPU_JOIN_BUDGET: Duration = Duration::from_secs(5);

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
    for device in devices {
        // Teardown may follow an operator pause. Keep Indicator's transition
        // validation strict while making repeated quiescence idempotent.
        if device.lifecycle_state() != Some(vmm_devices::IndicatedState::Pause)
        {
            device.pause();
        }
    }

    let report = vmm_devices::wait_all_quiesced(devices, QUIESCE_BUDGET);
    if !report.is_empty() {
        warn!(log, "devices did not quiesce before teardown";
            "devices" => ?report.stuck);
    }

    for device in devices {
        if let Err(error) =
            device.flush_backing(vmm_devices::FlushIntent::BestEffort)
        {
            warn!(log, "device flush incomplete at teardown";
                "device" => device.type_name(),
                "error" => ?error);
        }
    }

    report
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
    let outcome = match event_rx.recv() {
        Ok(VcpuEvent::Suspended { vcpu_id, kind }) => {
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
        Ok(VcpuEvent::Error {
            vcpu_id,
            error: err,
        }) => {
            error!(log, "vCPU error";
                "vcpu" => vcpu_id,
                "error" => &err,
            );
            Ok(RunOutcome::GuestFault)
        }
        Err(_) => {
            warn!(log, "all vCPU threads exited without events");
            Ok(RunOutcome::Halt)
        }
    };

    // A wedged backend must not prevent release of the bhyve instance.
    let _report = quiesce_devices(devices, log);

    if !join_vcpus_bounded(handles, VCPU_JOIN_BUDGET) {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use slog::Logger;
    use vmm_devices::Lifecycle as _;

    use super::{
        join_vcpus_bounded, quiesce_devices, RunOutcome, QUIESCE_BUDGET,
    };

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
}
