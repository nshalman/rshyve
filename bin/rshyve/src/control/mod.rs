// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unix domain socket control interface for vmadm integration.
//!
//! Listens on a path specified by `--control-socket` and accepts
//! newline-delimited JSON commands. Each command produces a single
//! JSON response line. Up to eight client connections are served concurrently.
//!
//! # Protocol
//!
//! Request:  `{"command":"status"}\n`
//! Response: `{"success":true,"state":"Running","vm_name":"testvm",...}\n`
//!
//! # Commands
//!
//! | Command       | Effect                                       |
//! |---------------|----------------------------------------------|
//! | `status`      | Returns VM state, name, CPU count, memory    |
//! | `pause`       | Pauses all vCPUs and kernel timers           |
//! | `resume`      | Resumes a paused VM                          |
//! | `shutdown`    | Graceful ACPI poweroff (VM_SUSPEND_POWEROFF) |
//! | `reset`       | Guest restart (VM_SUSPEND_RESET); VMM re-execs |
//! | `stop`        | Immediate VM halt (VM_SUSPEND_HALT)          |
//! | `device-list` | Lists the registered devices and their state |
//! | `device-add`    | Adds one `slot,driver[,config]` device     |
//! | `device-remove` | Asks the guest to give a device up         |
//! | `cpu-list`    | Lists every CPU slot and its state            |
//! | `cpu-add`     | Brings one more vCPU online                   |
//! | `mem-list`    | Lists the memory slots and the hot-add window |
//! | `mem-add`     | Gives the guest more memory, in bytes         |
//!
//! There is no `cpu-remove` and no `mem-remove`. illumos has no
//! `vm_deactivate_cpu` and no `VM_FREE_MEMSEG`, so neither removal is
//! possible at the kernel boundary. Both names are recognised and
//! refused with that reason, because an operator will try them and an
//! unknown-command error would read as a typo.

mod dispatch;
mod listener;
mod migrate;
mod protocol;

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use slog::{error, warn, Logger};

use vmm_core::cpuid::CpuBaseline;
use vmm_core::hdl::{SuspendHow, VmmHdl};
use vmm_core::mem::PhysMap;
use vmm_devices::quiesce::NamedDevice;
use vmm_devices::DeviceMigrateState;
use vmm_machine::signal::GuestStop;
use vmm_machine::{DeviceRegistry, HotplugEngines};

use crate::host_state::HostLocalState;

pub use listener::{spawn_control_thread, ControlListener};

// ── Error type ──────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

// ── VM state machine ────────────────────────────────────────────────

/// VM lifecycle state, stored as an `AtomicU8` for lock-free access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VmRunState {
    Running = 0,
    Paused = 1,
    Stopping = 2,
    Stopped = 3,
    Migrating = 4,
}

impl VmRunState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Running,
            1 => Self::Paused,
            2 => Self::Stopping,
            3 => Self::Stopped,
            4 => Self::Migrating,
            _other => Self::Stopped,
        }
    }
}

/// Shared VM state, readable from any thread without locking.
pub struct VmController {
    state: AtomicU8,
    hdl: Arc<VmmHdl>,
    physmap: Arc<PhysMap>,
    vm_name: String,
    num_cpus: u32,
    /// Every CPU slot the MADT describes. Equal to `num_cpus` unless
    /// `-c maxcpus=` asked for more.
    max_cpus: u32,
    mem_size: usize,
    start_time: Instant,
    migrate_status: Mutex<Option<Arc<Mutex<vmm_migrate::MigrationStatus>>>>,
    /// Per-vCPU metrics, shared with vCPU threads.
    vcpu_metrics: Vec<Arc<vmm_devices::metrics::VcpuMetrics>>,
    /// Every device in the machine. Read through on each request, not
    /// copied at startup, so a hotplug add or remove is visible to
    /// pause, resume and migration.
    registry: Arc<DeviceRegistry>,
    /// Set once the guest has been handed to another host. The source
    /// must never run the guest again: the destination owns the disk.
    migrated_away: AtomicBool,
    /// AHCI state has no migration payload, so attached media blocks migration.
    has_ahci_cd: bool,
    /// Each engine is `None` unless the VM was started with the option
    /// it needs, which is the only way the guest has an interface to
    /// answer on.
    hotplug: HotplugEngines,
    /// Serialises a change to the VM's CPU count or memory size against
    /// the start of a migration. Both are rare, so holding it costs
    /// nothing, and without it a migration can read the topology while
    /// a hot-add is still in flight and leave the addition behind.
    topology: Mutex<()>,
    /// Original CLI args for migrate-config reconstruction.
    cli_pci_slots: Vec<String>,
    cli_lpc: Vec<String>,
    /// CPU baseline for migration feature masking.
    cpu_baseline: CpuBaseline,
    host_local_state: HostLocalState,
    log: Logger,
}

impl VmController {
    // Construction mirrors the independently owned VM subsystems.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        hdl: Arc<VmmHdl>,
        physmap: Arc<PhysMap>,
        vm_name: String,
        num_cpus: u32,
        max_cpus: u32,
        mem_size: usize,
        vcpu_metrics: Vec<Arc<vmm_devices::metrics::VcpuMetrics>>,
        registry: Arc<DeviceRegistry>,
        has_ahci_cd: bool,
        hotplug: HotplugEngines,
        cli_pci_slots: Vec<String>,
        cli_lpc: Vec<String>,
        cpu_baseline: CpuBaseline,
        host_local_state: HostLocalState,
        log: Logger,
    ) -> Arc<Self> {
        let name = vm_name.to_string();
        Arc::new(Self {
            state: AtomicU8::new(VmRunState::Running as u8),
            hdl,
            physmap,
            vm_name: name,
            num_cpus,
            max_cpus,
            mem_size,
            start_time: Instant::now(),
            migrate_status: Mutex::new(None),
            vcpu_metrics,
            registry,
            migrated_away: AtomicBool::new(false),
            has_ahci_cd,
            hotplug,
            topology: Mutex::new(()),
            cli_pci_slots,
            cli_lpc,
            cpu_baseline,
            host_local_state,
            log,
        })
    }

    /// Lifecycle handles, cloned out of the registry so no registry lock
    /// is held while a device runs.
    fn lifecycle_devices(&self) -> Vec<Arc<dyn vmm_devices::Lifecycle>> {
        self.registry.lifecycle_devices()
    }

    /// The same set, each handle carrying its registry id so a stuck
    /// device is named by id and not by a type two devices share.
    fn named_devices(&self) -> Vec<NamedDevice> {
        vmm_machine::registry_named_devices(&self.registry)
    }

    fn export_device_queues(
        &self,
    ) -> Result<vmm_migrate::sync_source::DeviceQueueExport, String> {
        // Pause all devices so in-flight I/O completes before we
        // snapshot ring indices. Without this, requests popped from
        // the avail ring but not yet completed would be lost. The
        // guest would wait forever for their completions.
        let devices = self.named_devices();
        for named in &devices {
            // A device an operator already paused has stopped its
            // workers, so pausing it again only repeats that work.
            if named.device.lifecycle_state()
                != Some(vmm_devices::IndicatedState::Pause)
            {
                named.device.pause();
            }
        }
        let report = vmm_devices::quiesce::wait_all_quiesced_named(
            &devices,
            Duration::from_secs(30),
        );
        if !report.is_empty() {
            return Err(format!(
                "devices did not quiesce: {:?}",
                report.names()
            ));
        }

        let mut virtio_queues = Vec::new();
        let mut nvme_controllers = Vec::new();

        for dev in devices.iter().map(|named| &named.device) {
            match dev.export_migrate_state() {
                Some(DeviceMigrateState::VirtioQueues(queues)) => {
                    virtio_queues.extend(queues);
                }
                Some(DeviceMigrateState::Nvme(state)) => {
                    nvme_controllers.push(state);
                }
                None => {}
            }
        }
        Ok(vmm_migrate::sync_source::DeviceQueueExport {
            virtio_queues,
            nvme_controllers,
        })
    }

    fn state(&self) -> VmRunState {
        VmRunState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Whether this VM's guest now runs on another host.
    pub(super) fn migrated_away(&self) -> bool {
        self.migrated_away.load(Ordering::Acquire)
    }

    /// Record that the guest left, and arm the release that follows.
    ///
    /// A finished source is left paused, so nothing the guest does can
    /// end it and no other command reaches it. Without the timer an
    /// orchestrator that dies mid-switchover leaves the node holding
    /// the whole guest memory of a VM that runs nowhere.
    pub(super) fn finish_migrated_away(self: &Arc<Self>, grace: Duration) {
        self.migrated_away.store(true, Ordering::Release);
        self.state
            .store(VmRunState::Stopped as u8, Ordering::Release);

        let ctrl = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("migrated-source-release".into())
            .spawn(move || {
                thread::sleep(grace);
                if !grace_must_release(ctrl.state()) {
                    return;
                }
                warn!(ctrl.log, "releasing a migrated source nobody reaped";
                    "grace_secs" => grace.as_secs());
                ctrl.log_failed_release();
            });

        if let Err(e) = spawned {
            // No timer means no reaper, so release now. Leaking the
            // guest's memory is worse than an early release of a VM
            // whose guest has already gone.
            error!(self.log, "no release timer for a migrated source";
                "error" => %e);
            self.log_failed_release();
        }
    }

    /// End a VM whose guest now runs on another host.
    ///
    /// A caller that does not win [`VmController::begin_stop`] leaves
    /// the release to the one that did.
    pub(super) fn release_migrated_source(&self) -> io::Result<()> {
        if !self.begin_stop() {
            return Ok(());
        }
        release_paused_vm(
            || self.hdl.suspend(SuspendHow::Halt, 0).map(|_| ()),
            || self.hdl.resume(),
        )
    }

    /// Move to `Stopping`, and say whether this caller is the one that
    /// must resume a parked VM before its vCPUs can see the stop.
    ///
    /// The compare-and-swap is what makes the resume happen once,
    /// however many stops arrive at once. A second resume would meet
    /// an instance that is not paused, which is a kernel level
    /// mistake and not a userspace one.
    pub(super) fn begin_stop(&self) -> bool {
        let from = self.state();
        let parked = stop_needs_resume(from, self.migrated_away())
            && self.transition(from, VmRunState::Stopping);
        if !parked {
            self.state
                .store(VmRunState::Stopping as u8, Ordering::Release);
        }
        parked
    }

    /// Latch `how`, then let a parked VM see it.
    fn stop_for_signal(&self, how: SuspendHow) -> io::Result<()> {
        if !self.begin_stop() {
            return self.hdl.suspend(how, 0).map(|_| ());
        }
        release_paused_vm(
            || self.hdl.suspend(how, 0).map(|_| ()),
            || self.hdl.resume(),
        )
    }

    /// The same release from a path with nobody to hand an error to.
    fn log_failed_release(&self) {
        if let Err(e) = self.release_migrated_source() {
            error!(self.log, "failed to release a migrated source";
                "error" => %e);
        }
    }

    /// Hold the topology still.
    ///
    /// A hot-add takes this and so does the start of a migration, which
    /// is what stops one landing between the state change and the
    /// topology read.
    fn lock_topology(&self) -> std::sync::MutexGuard<'_, ()> {
        // No panic on a control path. The value is a unit, which a
        // panic elsewhere cannot leave half written, so the poison is
        // taken rather than dropping the request.
        self.topology.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Attempt a state transition via compare-and-swap.
    fn transition(&self, from: VmRunState, to: VmRunState) -> bool {
        self.state
            .compare_exchange(
                from as u8,
                to as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Pause all devices: signal each to quiesce, then wait for all
    /// background workers to drain and park.
    fn pause_devices(&self) -> Result<(), Vec<String>> {
        pause_device_set(&self.named_devices(), Duration::from_secs(5))
    }

    /// Resume all devices: wake parked workers.
    fn resume_devices(&self) {
        resume_device_set(&self.named_devices());
    }
}

/// SIGTERM shutdown driven through the control plane.
///
/// Only the control plane knows whether the VM is parked on the
/// kernel's pause. A stop that is latched but never followed by a
/// resume leaves a paused VM running for ever, which is why SIGTERM
/// does nothing to one today.
struct ControlStop(Arc<VmController>);

/// The stop a VM with a control socket uses for SIGTERM.
pub(crate) fn sigterm_stop(ctrl: Arc<VmController>) -> Arc<dyn GuestStop> {
    Arc::new(ControlStop(ctrl))
}

impl GuestStop for ControlStop {
    fn request_poweroff(&self) -> io::Result<()> {
        self.0.stop_for_signal(SuspendHow::PowerOff)
    }

    fn halt(&self) -> io::Result<()> {
        self.0.stop_for_signal(SuspendHow::Halt)
    }
}

/// Whether a stop must resume the VM before its vCPUs can see it.
///
/// Two states leave the vCPU threads parked on the kernel's pause: an
/// operator pause, and a source whose guest migrated away. A parked
/// thread retries VM_RUN against EBUSY and never reaches the suspend
/// check, so nothing it is told lands until the instance runs again.
fn stop_needs_resume(state: VmRunState, migrated_away: bool) -> bool {
    match state {
        VmRunState::Paused => true,
        VmRunState::Stopped => migrated_away,
        _ => false,
    }
}

/// Whether the grace timer must still release the VM itself.
///
/// False once something else moved it on, which is the operator `stop`
/// that beat the timer to it.
fn grace_must_release(state: VmRunState) -> bool {
    state == VmRunState::Stopped
}

/// Stop a paused VM without letting its guest run one more instruction.
///
/// The suspend is latched FIRST. A paused vCPU thread is retrying
/// VM_RUN against EBUSY; the resume lets it back into the kernel, which
/// sees the latched suspend and turns it straight around. Resuming
/// first would run guest code against a disk the migration destination
/// already owns. A suspend that fails is not followed by a resume, for
/// the same reason.
fn release_paused_vm<E>(
    suspend: impl FnOnce() -> Result<(), E>,
    resume: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    suspend()?;
    resume()
}

fn pause_device_set(
    devices: &[NamedDevice],
    budget: Duration,
) -> Result<(), Vec<String>> {
    for named in devices {
        if named.device.lifecycle_state()
            != Some(vmm_devices::IndicatedState::Pause)
        {
            named.device.pause();
        }
    }
    let report = vmm_devices::quiesce::wait_all_quiesced_named(devices, budget);
    if !report.is_empty() {
        resume_device_set(devices);
        // Named, so an operator can tell two devices of one type apart.
        return Err(report.names());
    }
    Ok(())
}

fn resume_device_set(devices: &[NamedDevice]) {
    for named in devices {
        if matches!(
            named.device.lifecycle_state(),
            None | Some(vmm_devices::IndicatedState::Pause)
        ) {
            named.device.resume();
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    struct NeverQuiesces {
        paused: AtomicBool,
        resume_calls: AtomicUsize,
    }

    impl vmm_devices::Lifecycle for NeverQuiesces {
        fn type_name(&self) -> &'static str {
            "never"
        }

        fn lifecycle_state(&self) -> Option<vmm_devices::IndicatedState> {
            Some(if self.paused.load(Ordering::Acquire) {
                vmm_devices::IndicatedState::Pause
            } else {
                vmm_devices::IndicatedState::Run
            })
        }

        fn pause(&self) {
            self.paused.store(true, Ordering::Release);
        }

        fn resume(&self) {
            self.resume_calls.fetch_add(1, Ordering::AcqRel);
            self.paused.store(false, Ordering::Release);
        }

        fn is_quiesced(&self) -> bool {
            false
        }
    }

    fn never_quiesces() -> Arc<NeverQuiesces> {
        Arc::new(NeverQuiesces {
            paused: AtomicBool::new(false),
            resume_calls: AtomicUsize::new(0),
        })
    }

    fn named(id: Option<&str>, device: Arc<NeverQuiesces>) -> Vec<NamedDevice> {
        vec![NamedDevice {
            id: id.map(str::to_string),
            device,
        }]
    }

    #[test]
    fn pause_devices_times_out_and_resumes_devices() {
        let device = never_quiesces();
        let devices = named(None, device.clone());

        let result = pause_device_set(&devices, Duration::from_millis(20));

        assert_eq!(result, Err(vec!["never".to_string()]));
        assert!(!device.paused.load(Ordering::Acquire));
        assert_eq!(device.resume_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn a_stuck_device_is_reported_by_its_registry_id() {
        // Two devices of one type are indistinguishable by type name, so
        // the operator-facing report carries the id.
        let devices = named(Some("virtio-blk@4"), never_quiesces());

        let result = pause_device_set(&devices, Duration::from_millis(20));

        assert_eq!(result, Err(vec!["never (virtio-blk@4)".to_string()]));
    }

    #[test]
    fn resume_devices_skips_tracked_devices_that_are_running() {
        let device = never_quiesces();
        let devices = named(None, device.clone());

        resume_device_set(&devices);

        assert_eq!(device.resume_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_release_latches_the_suspend_before_the_resume() {
        // Reversed, the guest runs against a disk the destination owns.
        let order = Mutex::new(Vec::new());

        release_paused_vm::<()>(
            || {
                order.lock().expect("order lock").push("suspend");
                Ok(())
            },
            || {
                order.lock().expect("order lock").push("resume");
                Ok(())
            },
        )
        .expect("both steps succeed");

        assert_eq!(*order.lock().expect("order lock"), ["suspend", "resume"]);
    }

    #[test]
    fn a_failed_suspend_never_resumes_the_guest() {
        let resumed = AtomicBool::new(false);

        let result = release_paused_vm(
            || Err("suspend ioctl failed"),
            || {
                resumed.store(true, Ordering::Release);
                Ok(())
            },
        );

        assert_eq!(result, Err("suspend ioctl failed"));
        assert!(!resumed.load(Ordering::Acquire));
    }

    #[test]
    fn the_grace_timer_stands_down_once_something_else_acted() {
        assert!(grace_must_release(VmRunState::Stopped));
        for state in [
            VmRunState::Running,
            VmRunState::Paused,
            VmRunState::Stopping,
            VmRunState::Migrating,
        ] {
            assert!(!grace_must_release(state), "{state:?}");
        }
    }

    #[test]
    fn a_stop_resumes_only_a_vm_that_is_parked() {
        // A resume of an instance that is not paused is a kernel level
        // mistake, so this must be exact.
        assert!(stop_needs_resume(VmRunState::Paused, false));
        assert!(stop_needs_resume(VmRunState::Stopped, true));

        assert!(!stop_needs_resume(VmRunState::Stopped, false));
        for state in [
            VmRunState::Running,
            VmRunState::Stopping,
            VmRunState::Migrating,
        ] {
            assert!(!stop_needs_resume(state, false), "{state:?}");
            assert!(!stop_needs_resume(state, true), "{state:?}");
        }
    }

    #[test]
    fn vm_state_roundtrip() {
        for state in [
            VmRunState::Running,
            VmRunState::Paused,
            VmRunState::Stopping,
            VmRunState::Stopped,
            VmRunState::Migrating,
        ] {
            assert_eq!(VmRunState::from_u8(state as u8), state);
        }
    }

    #[test]
    fn vm_state_invalid_defaults_to_stopped() {
        assert_eq!(VmRunState::from_u8(255), VmRunState::Stopped);
    }

    #[test]
    fn state_serialize_lowercase() {
        let json = serde_json::to_string(&VmRunState::Running).unwrap();
        assert_eq!(json, r#""running""#);
        let json = serde_json::to_string(&VmRunState::Paused).unwrap();
        assert_eq!(json, r#""paused""#);
        let json = serde_json::to_string(&VmRunState::Migrating).unwrap();
        assert_eq!(json, r#""migrating""#);
    }
}
