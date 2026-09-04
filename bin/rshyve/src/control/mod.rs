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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use slog::Logger;

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::PhysMap;
use vmm_devices::quiesce::NamedDevice;
use vmm_machine::{DeviceRegistry, HotplugEngines};

use crate::host_state::HostLocalState;

pub use listener::spawn_control_thread;

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
    cpu_baseline: String,
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
        cpu_baseline: String,
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
            // An Indicator refuses a Pause to Pause transition, so a
            // device an operator already paused must not be paused
            // again.
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
            for q in dev.export_virtio_state() {
                virtio_queues.push(vmm_migrate::codec::VirtioQueueState {
                    pci_slot: 0,
                    queue_idx: q.queue_idx,
                    queue_size: q.queue_size,
                    desc_addr: q.desc_addr,
                    avail_addr: q.avail_addr,
                    used_addr: q.used_addr,
                    enabled: q.enabled,
                    features: q.features,
                    avail_idx: q.avail_idx,
                    used_idx: q.used_idx,
                    msix_addr: q.msix_addr,
                    msix_data: q.msix_data,
                    msix_vector: q.msix_vector,
                    pci_state: q.pci_state.clone().map(|pci| {
                        vmm_migrate::codec::VirtioMigratePciState {
                            command: pci.command,
                            bar_addrs: pci.bar_addrs,
                        }
                    }),
                });
            }
            if let Some(nvme) = dev.export_nvme_state() {
                nvme_controllers.push(
                    vmm_migrate::codec::NvmeControllerState {
                        cc: nvme.cc,
                        csts: nvme.csts,
                        aqa: nvme.aqa,
                        asq_base: nvme.asq_base,
                        acq_base: nvme.acq_base,
                        admin_sq: nvme.admin_sq.map(|sq| {
                            vmm_migrate::codec::NvmeSqState {
                                base: sq.base,
                                size: sq.size,
                                head: sq.head,
                                tail: sq.tail,
                                cq_id: sq.cq_id,
                            }
                        }),
                        admin_cq: nvme.admin_cq.map(|cq| {
                            vmm_migrate::codec::NvmeCqState {
                                base: cq.base,
                                size: cq.size,
                                head: cq.head,
                                tail: cq.tail,
                                phase: cq.phase,
                                iv: cq.iv,
                            }
                        }),
                        io_sqs: nvme
                            .io_sqs
                            .into_iter()
                            .map(|sq| {
                                sq.map(|sq| vmm_migrate::codec::NvmeSqState {
                                    base: sq.base,
                                    size: sq.size,
                                    head: sq.head,
                                    tail: sq.tail,
                                    cq_id: sq.cq_id,
                                })
                            })
                            .collect(),
                        io_cqs: nvme
                            .io_cqs
                            .into_iter()
                            .map(|cq| {
                                cq.map(|cq| vmm_migrate::codec::NvmeCqState {
                                    base: cq.base,
                                    size: cq.size,
                                    head: cq.head,
                                    tail: cq.tail,
                                    phase: cq.phase,
                                    iv: cq.iv,
                                })
                            })
                            .collect(),
                        num_io_queues: nvme.num_io_queues,
                        pci_command: nvme.pci_command,
                        pci_bar_addrs: nvme.pci_bar_addrs,
                        msix_entries: nvme.msix_entries,
                        msix_enabled: nvme.msix_enabled,
                    },
                );
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

    /// Halt all devices: permanent shutdown.
    // Kept for the pending graceful device-stop sequence.
    #[allow(dead_code)]
    fn halt_devices(&self) {
        for named in self.named_devices() {
            named.device.halt();
        }
    }
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
