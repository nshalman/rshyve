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
//! | `resume`      | Resumes a paused VM; `force` for one in doubt |
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
mod handover;
mod listener;
mod migrate;
mod protocol;
mod reap;
mod run_state;

/// How long the migration pause waits for every device worker to park.
///
/// The guest is still running while this runs, so a generous budget
/// costs nothing but a slower switchover.
const MIGRATION_QUIESCE_BUDGET: Duration = Duration::from_secs(30);

/// How long an operator pause waits for the device workers to park.
const DEVICE_PAUSE_BUDGET: Duration = Duration::from_secs(5);

/// One device, with the identity the migration wire names it by.
pub(super) struct MigrateDevice {
    pub(super) ident: vmm_migrate::codec::DeviceIdentity,
    pub(super) named: NamedDevice,
}

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use slog::{error, info, warn, Logger};

use vmm_core::cpuid::CpuBaseline;
use vmm_core::hdl::{SuspendHow, VmmHdl};
use vmm_core::mem::PhysMap;
use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStateError, DeviceStatePayload, WireBdf,
};
use vmm_devices::quiesce::NamedDevice;
use vmm_machine::signal::GuestStop;
use vmm_machine::{DeviceRegistry, HotplugEngines, SUSPEND_SOURCE_VMM};

use crate::host_state::HostLocalState;

pub use listener::{spawn_control_thread, ControlListener};
pub(crate) use migrate::device_blockers;
use run_state::{LeaseOwner, StepFailed, StopFailure};

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
    /// A release failed and the devices could not be parked again.
    /// Only a stop is taken.
    Faulted = 5,
    #[serde(rename = "import-failed")]
    ImportFailed = 6,
    #[serde(rename = "import-in-doubt")]
    ImportInDoubt = 7,
    /// An import the source committed to, whose last step failed. The
    /// guest is here and nowhere else.
    #[serde(rename = "import-committed")]
    ImportCommitted = 8,
}

impl VmRunState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Running,
            1 => Self::Paused,
            2 => Self::Stopping,
            3 => Self::Stopped,
            4 => Self::Migrating,
            5 => Self::Faulted,
            6 => Self::ImportFailed,
            7 => Self::ImportInDoubt,
            8 => Self::ImportCommitted,
            _other => Self::Stopped,
        }
    }
}

/// Shared VM state, readable from any thread without locking.
pub struct VmController {
    run: run_state::RunControl,
    hdl: Arc<VmmHdl>,
    /// The pause `Command::Pause` and a migration take.
    ///
    /// Shared with the PCI hot-add engine, because the kernel keeps one
    /// pause flag per instance and the engine pauses the whole VM for
    /// each bus change. The guest picks when that happens, so without
    /// the shared count an operator `pause` landing inside an eject was
    /// answered `EALREADY` and reported as failed.
    pause_gate: Arc<vmm_machine::VmPauseGate>,
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
    release_stuck: AtomicBool,
    /// AHCI state has no migration payload, so attached media blocks migration.
    has_ahci_cd: bool,
    /// Each engine is `None` unless the VM was started with the option
    /// it needs, which is the only way the guest has an interface to
    /// answer on.
    hotplug: HotplugEngines,
    /// Original CLI args for migrate-config reconstruction.
    cli_pci_slots: Vec<String>,
    cli_lpc: Vec<String>,
    /// CPU baseline for migration feature masking.
    cpu_baseline: CpuBaseline,
    /// The Hyper-V enlightenment, for a VM started with `--hyperv`.
    /// Its MSR state is migrated; nothing else touches it here.
    hyperv: Option<Arc<vmm_hyperv::HyperV>>,
    host_local_state: HostLocalState,
    /// The last migration took ring engines that cannot prove a fresh
    /// attach is new.
    allow_unproven: AtomicBool,
    /// What `migrate-status` reports for the ring disks, once a
    /// migration moved them.
    rings: Mutex<Option<&'static str>>,
    log: Logger,
}

/// What this VM holds that only migration cares about.
///
/// Each one either blocks a migration or travels on the wire, so they
/// arrive together rather than as three more constructor arguments.
pub(crate) struct MigrationInputs {
    /// AHCI state has no migration payload, so attached media blocks
    /// migration.
    pub(crate) has_ahci_cd: bool,
    /// The Hyper-V enlightenment, for a VM started with `--hyperv`.
    pub(crate) hyperv: Option<Arc<vmm_hyperv::HyperV>>,
    /// State that lives on this host and cannot follow the guest.
    pub(crate) host_local_state: HostLocalState,
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
        hotplug: HotplugEngines,
        cli_pci_slots: Vec<String>,
        cli_lpc: Vec<String>,
        cpu_baseline: CpuBaseline,
        migration: MigrationInputs,
        log: Logger,
    ) -> Arc<Self> {
        let name = vm_name.to_string();
        let pause_gate = hotplug.pause_gate(&hdl);
        Arc::new(Self {
            run: run_state::RunControl::with_topology(
                VmRunState::Running,
                hotplug.topology(),
            ),
            hdl,
            pause_gate,
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
            release_stuck: AtomicBool::new(false),
            has_ahci_cd: migration.has_ahci_cd,
            hotplug,
            cli_pci_slots,
            cli_lpc,
            cpu_baseline,
            hyperv: migration.hyperv,
            host_local_state: migration.host_local_state,
            allow_unproven: AtomicBool::new(false),
            rings: Mutex::new(None),
            log,
        })
    }

    /// The same set, each handle carrying its registry id so a stuck
    /// device is named by id and not by a type two devices share.
    fn named_devices(&self) -> Vec<NamedDevice> {
        vmm_machine::registry_named_devices(&self.registry)
    }

    /// Every device a migration carries state for, with the PCI address
    /// that identifies it on the wire.
    ///
    /// A device without an address has nothing the wire can name, and
    /// one without a lifecycle handle has nothing to export.
    pub(super) fn migrate_devices(&self) -> Vec<MigrateDevice> {
        self.registry
            .list()
            .into_iter()
            .filter_map(|slot| {
                let bdf = slot.bdf?;
                let device = slot.lifecycle?;
                Some(MigrateDevice {
                    ident: vmm_migrate::codec::DeviceIdentity {
                        bdf: bdf.into(),
                        kind: device.type_name().to_string(),
                    },
                    named: NamedDevice {
                        id: Some(slot.id),
                        device,
                    },
                })
            })
            .collect()
    }

    /// The device identity set the preamble carries.
    pub(super) fn migrate_identities(
        &self,
    ) -> Vec<vmm_migrate::codec::DeviceIdentity> {
        self.migrate_devices()
            .into_iter()
            .map(|d| d.ident)
            .collect()
    }

    /// Stop every device worker and every kernel ring, then wait for
    /// them to quiesce.
    ///
    /// Runs before the vCPUs pause. In-flight I/O has to complete
    /// first: a request popped from the avail ring and not completed is
    /// a completion the guest waits for for ever. Every thread that
    /// writes guest memory has to be stopped as well, or the final
    /// dirty pass ships a used ring the destination never sees filled.
    fn pause_devices_for_migration(&self) -> Result<(), DeviceStateError> {
        let devices = self.named_devices();
        for named in &devices {
            // A device an operator already paused has stopped its
            // workers, so pausing it again only repeats that work.
            if named.device.lifecycle_state()
                != Some(vmm_devices::IndicatedState::Pause)
            {
                named.device.pause_within(MIGRATION_QUIESCE_BUDGET);
            }
        }
        let report = vmm_devices::quiesce::wait_all_quiesced_named(
            &devices,
            MIGRATION_QUIESCE_BUDGET,
        );
        if !report.is_empty() {
            return Err(DeviceStateError::Export(format!(
                "devices did not quiesce: {:?}",
                report.names()
            )));
        }
        // The kernel rings last: a ring paused before its device's
        // workers stop would still be asked for descriptors.
        for named in &devices {
            named.device.pause_for_migration()?;
        }
        Ok(())
    }

    /// Make every backing store durable.
    ///
    /// zvol write caching means writes the guest was told were complete
    /// may still sit in cache, and `zfs send` ships only committed pool
    /// state.
    fn flush_devices_for_migration(
        &self,
        left: &vmm_migrate::wait::Abandoned,
    ) -> Result<(), vmm_migrate::source::DeviceFlushError> {
        use vmm_machine::OutsideFlushError as Outside;
        use vmm_migrate::source::DeviceFlushError;
        vmm_machine::flush_durable(&self.registry, || left.get()).map_err(
            |error| match error {
                Outside::Device { device, source } => {
                    DeviceFlushError::Device { device, source }
                }
                other => DeviceFlushError::Stopped(other.to_string()),
            },
        )
    }

    /// Read every device's state out. A pure read: the pause already
    /// happened.
    fn export_device_state(
        &self,
    ) -> Result<Vec<DeviceStatePayload>, DeviceStateError> {
        self.migrate_devices()
            .into_iter()
            .map(|d| {
                Ok(DeviceStatePayload {
                    bdf: d.ident.bdf,
                    state: d.named.device.export_migrate_state()?,
                })
            })
            .collect()
    }

    /// The Hyper-V enlightenment a migration carries, with its overlay
    /// pages dropped first so the RAM image holds the guest's bytes.
    fn export_hyperv(
        &self,
    ) -> Option<vmm_devices::lifecycle::HypervMigrateState> {
        let hyperv = self.hyperv.as_ref()?;
        hyperv.pause_overlays();
        Some(hyperv.export_state())
    }

    /// Put a source's enlightenment back. `false` when this VM has
    /// none, which makes the payload a mismatch rather than a silent
    /// drop.
    fn restore_hyperv(
        &self,
        state: &vmm_devices::lifecycle::HypervMigrateState,
    ) -> bool {
        match self.hyperv.as_ref() {
            Some(hyperv) => {
                hyperv.import_state(state);
                true
            }
            None => false,
        }
    }

    /// Give one device the state its counterpart on the source exported.
    fn restore_device_state(
        &self,
        bdf: WireBdf,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        let device = self
            .migrate_devices()
            .into_iter()
            .find(|d| d.ident.bdf == bdf)
            .ok_or_else(|| {
                DeviceStateError::Invalid(format!("no device at {bdf}"))
            })?;
        device.named.device.restore_migrate_state(state)
    }

    /// Put the devices back to work after a migration that failed once
    /// the guest was already paused.
    ///
    /// A resume alone leaves a kernel ring stopped, and viona answers a
    /// guest kick on a stopped ring with EBUSY, so the guest would come
    /// back with a dead NIC.
    fn resume_devices_after_migration(&self) {
        if let Some(hyperv) = &self.hyperv {
            hyperv.resume_overlays();
        }
        let devices = self.named_devices();
        resume_device_set(&devices);
        for named in &devices {
            if let Err(error) = named.device.resume_after_migration() {
                error!(self.log, "a device's rings stayed paused after a \
                    failed migration; the guest may see it dead";
                    "device" => named.device.type_name(),
                    "error" => %error);
            }
        }
    }

    fn state(&self) -> VmRunState {
        self.run.get()
    }

    /// Whether the migration's phase says the source committed.
    fn source_committed(&self) -> bool {
        self.migrate_status.lock().ok().is_some_and(|ms| {
            ms.as_ref().is_some_and(|s| {
                s.lock().is_ok_and(|s| {
                    s.phase == vmm_migrate::MigrationPhase::Committed
                })
            })
        })
    }

    /// Whether this VM's guest now runs on another host.
    pub(super) fn migrated_away(&self) -> bool {
        self.migrated_away.load(Ordering::Acquire)
    }

    /// Record that the guest left, then release the VM if nobody has
    /// within `grace`. Runs on the migration thread.
    ///
    /// A finished source is left paused, so nothing the guest does can
    /// end it and no other command reaches it. Without the release an
    /// orchestrator that dies mid-switchover leaves the node holding
    /// the whole guest memory of a VM that runs nowhere.
    pub(super) fn finish_migrated_away(&self, grace: Duration) {
        self.migrated_away.store(true, Ordering::Release);
        let handed = self.run.hand_off(|| self.release_pause());
        if !self.ended(handed) {
            return;
        }
        thread::sleep(grace);
        if !self.run.must_reap() {
            return;
        }
        warn!(self.log, "releasing a migrated source nobody reaped";
            "grace_secs" => grace.as_secs());
        self.retry_release();
    }

    /// Report how a migration's end went. False when a stop owns the
    /// VM; a release the stop left that failed is retried here.
    pub(super) fn ended(&self, end: anyhow::Result<bool>) -> bool {
        match end {
            Ok(ended) => {
                if !ended {
                    info!(self.log, "a stop arrived during the migration; \
                        it owns the VM"; "state" => ?self.state());
                }
                ended
            }
            Err(e) => {
                error!(self.log, "a stopping VM would not resume to see \
                    its stop"; "error" => %e);
                self.retry_release();
                false
            }
        }
    }

    fn retry_release(&self) {
        reap::reap(
            reap::CADENCE,
            || self.run.must_reap(),
            || self.release_migrated_source(),
            |stuck| self.release_stuck.store(stuck, Ordering::Release),
            vmm_machine::terminate_wedged,
            &self.log,
        );
    }

    /// A held pause the reaper has failed to give back past its fast
    /// tries. The VM may sit in `stopping` until it works.
    fn release_stuck(&self) -> bool {
        // The reaper sees a release made elsewhere only between retries.
        // A busy lock is likely the hung release itself, so status must
        // not wait for it.
        self.release_stuck.load(Ordering::Acquire)
            && self.run.try_must_reap().unwrap_or(true)
    }

    /// End a VM whose guest now runs on another host, or give back a
    /// pause a stop left held.
    pub(super) fn release_migrated_source(&self) -> io::Result<()> {
        self.latch_stop(SuspendHow::Halt, |_| true)
            .map(drop)
            .map_err(Into::into)
    }

    /// Latch `how` on a VM in a state `allowed` takes, and report the
    /// state the VM is really in.
    ///
    /// A VM holding the pause is suspended first and resumed second,
    /// which is the order [`run_state::release_paused_vm`] documents.
    fn latch_stop(
        &self,
        how: SuspendHow,
        allowed: impl FnOnce(VmRunState) -> bool,
    ) -> Result<VmRunState, StopFailure<io::Error>> {
        self.run
            .stop_from(
                run_state::STOP_WAIT,
                allowed,
                || self.hold_pause().map_err(io::Error::other),
                || self.hdl.suspend(how, SUSPEND_SOURCE_VMM).map(drop),
                || self.release_pause().map_err(io::Error::other),
            )
            .map(|()| VmRunState::Stopping)
    }

    /// Latch `how` from a signal handler's stop.
    fn stop_for_signal(&self, how: SuspendHow) -> io::Result<()> {
        self.latch_stop(how, |_| true).map(drop).map_err(Into::into)
    }

    /// Take this controller's hold on the kernel pause.
    fn hold_pause(&self) -> anyhow::Result<()> {
        vmm_machine::VmPause::pause(self.pause_gate.as_ref())
    }

    /// Give up this controller's hold. The instance runs again once the
    /// hot-unplug path has given up any hold of its own.
    fn release_pause(&self) -> anyhow::Result<()> {
        vmm_machine::VmPause::resume(self.pause_gate.as_ref())
    }

    /// A migration's vCPU pause, through the gate every other pause
    /// shares. Refused once a stop owns the VM.
    fn pause_for_migration(&self) -> io::Result<()> {
        self.run
            .park_migration(|| self.hold_pause())
            .map_err(|e| match e {
                Some(e) => io::Error::other(e),
                None => io::Error::other(run_state::STOP_OWNS_VM),
            })
    }

    /// A migration's vCPU release, after its devices were resumed.
    /// Refused once a stop owns the VM.
    fn resume_for_migration(&self) -> io::Result<()> {
        self.run
            .release_migration(
                || self.release_pause(),
                || self.repark_devices(),
            )
            .map_err(|e| match e {
                Some(e) => io::Error::other(e),
                None => io::Error::other(run_state::STOP_OWNS_VM),
            })
    }

    /// Park the devices again under a pause that would not let go.
    fn repark_devices(&self) -> bool {
        match pause_device_set(&self.named_devices(), DEVICE_PAUSE_BUDGET) {
            Ok(()) => true,
            Err(stuck) => {
                error!(self.log, "devices run under a held pause; the VM \
                    is faulted until a stop"; "stuck" => ?stuck);
                false
            }
        }
    }

    /// Resume all devices: wake parked workers.
    fn resume_devices(&self) {
        resume_device_set(&self.named_devices());
    }
}

impl LeaseOwner for VmController {
    fn abandon(&self) {
        error!(
            self.log,
            "a migration thread ended without closing its \
            claim"
        );
        let end = self.run.abandon_migration(run_state::Abandoned {
            confirmed: self.source_committed(),
            resume_devices: || self.resume_devices_after_migration(),
            release: || self.release_pause(),
        });
        self.ended(end.map(|()| true));
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

/// Park the devices, then take the kernel pause.
fn park_all(
    devices: &[NamedDevice],
    budget: Duration,
    hold: impl FnOnce() -> anyhow::Result<()>,
    held: &mut bool,
    log: &Logger,
) -> Result<(), StepFailed<String>> {
    if let Err(stuck) = pause_device_set(devices, budget) {
        return Err(StepFailed::Undone(format!(
            "device pause timed out: {stuck:?}"
        )));
    }
    if let Err(e) = hold() {
        resume_device_set(devices);
        error!(log, "pause failed"; "error" => %e);
        return Err(StepFailed::Undone("pause ioctl failed".to_string()));
    }
    *held = true;
    Ok(())
}

/// Device workers resume before the vCPUs so interrupts can be
/// delivered. A release that fails parks them again; if they will not
/// park, the VM is faulted rather than called paused.
fn unpark_all(
    devices: &[NamedDevice],
    budget: Duration,
    release: impl FnOnce() -> anyhow::Result<()>,
    held: &mut bool,
    log: &Logger,
) -> Result<(), StepFailed<String>> {
    resume_device_set(devices);
    if let Err(e) = release() {
        error!(log, "resume failed"; "error" => %e);
        if let Err(stuck) = pause_device_set(devices, budget) {
            error!(log, "devices run under a held pause"; "stuck" => ?stuck);
            return Err(StepFailed::Faulted(
                "resume ioctl failed and the devices would not park again; \
                 the VM is faulted and only a stop is taken"
                    .to_string(),
            ));
        }
        return Err(StepFailed::Undone("resume ioctl failed".to_string()));
    }
    *held = false;
    Ok(())
}

fn pause_device_set(
    devices: &[NamedDevice],
    budget: Duration,
) -> Result<(), Vec<String>> {
    for named in devices {
        if named.device.lifecycle_state()
            != Some(vmm_devices::IndicatedState::Pause)
        {
            named.device.pause_within(budget);
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
    use super::run_state::release_paused_vm;
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    struct StubDevice {
        quiesces: bool,
        paused: AtomicBool,
        resume_calls: AtomicUsize,
    }

    impl vmm_devices::Lifecycle for StubDevice {
        fn type_name(&self) -> &'static str {
            if self.quiesces {
                "parks"
            } else {
                "never"
            }
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
            self.quiesces && self.paused.load(Ordering::Acquire)
        }
    }

    fn stub(quiesces: bool) -> Arc<StubDevice> {
        Arc::new(StubDevice {
            quiesces,
            paused: AtomicBool::new(false),
            resume_calls: AtomicUsize::new(0),
        })
    }

    fn never_quiesces() -> Arc<StubDevice> {
        stub(false)
    }

    fn named(id: Option<&str>, device: Arc<StubDevice>) -> Vec<NamedDevice> {
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

        assert_eq!(
            result,
            Err(StopFailure::NotLatched("suspend ioctl failed"))
        );
        assert!(!resumed.load(Ordering::Acquire));
    }

    /// A latched stop must not be reported as a VM that never got one.
    #[test]
    fn a_failed_resume_still_says_the_suspend_latched() {
        let result = release_paused_vm::<&str>(|| Ok(()), || Err("EBUSY"));

        assert_eq!(result, Err(StopFailure::NotReleased("EBUSY")));
    }

    fn discard() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn a_failed_release_parks_the_devices_again() {
        // The state goes back to Paused, and an import trusts Paused to
        // mean no device worker runs.
        let device = stub(true);
        let devices = named(None, device.clone());
        let mut held = false;
        park_all(
            &devices,
            Duration::from_secs(1),
            || Ok(()),
            &mut held,
            &discard(),
        )
        .expect("parked");
        assert!(held);

        let failed = unpark_all(
            &devices,
            Duration::from_secs(1),
            || anyhow::bail!("VM_RESUME failed"),
            &mut held,
            &discard(),
        );

        assert_eq!(
            failed,
            Err(StepFailed::Undone("resume ioctl failed".to_string()))
        );
        assert!(held, "the pause is still ours");
        assert!(device.paused.load(Ordering::Acquire), "device left running");
    }

    #[test]
    fn devices_that_will_not_park_again_fault_the_vm() {
        // Paused would let an import write into running devices.
        let device = never_quiesces();
        let devices = named(None, device.clone());
        let mut held = true;

        let failed = unpark_all(
            &devices,
            Duration::from_millis(20),
            || anyhow::bail!("VM_RESUME failed"),
            &mut held,
            &discard(),
        );

        assert!(matches!(failed, Err(StepFailed::Faulted(_))), "{failed:?}");
        assert!(held);
    }

    #[test]
    fn a_release_gives_the_pause_back_and_wakes_the_devices() {
        let device = stub(true);
        let devices = named(None, device.clone());
        let mut held = true;
        device.paused.store(true, Ordering::Release);

        unpark_all(
            &devices,
            Duration::from_secs(1),
            || Ok(()),
            &mut held,
            &discard(),
        )
        .expect("released");

        assert!(!held);
        assert!(!device.paused.load(Ordering::Acquire));
    }

    #[test]
    fn vm_state_roundtrip() {
        for state in [
            VmRunState::Running,
            VmRunState::Paused,
            VmRunState::Stopping,
            VmRunState::Stopped,
            VmRunState::Migrating,
            VmRunState::Faulted,
            VmRunState::ImportFailed,
            VmRunState::ImportInDoubt,
            VmRunState::ImportCommitted,
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
        let json = serde_json::to_string(&VmRunState::ImportFailed).unwrap();
        assert_eq!(json, r#""import-failed""#);
        let json = serde_json::to_string(&VmRunState::ImportInDoubt).unwrap();
        assert_eq!(json, r#""import-in-doubt""#);
        let json = serde_json::to_string(&VmRunState::ImportCommitted).unwrap();
        assert_eq!(json, r#""import-committed""#);
    }
}
