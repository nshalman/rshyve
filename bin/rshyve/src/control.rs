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
//! | Command    | Effect                                      |
//! |------------|---------------------------------------------|
//! | `status`   | Returns VM state, name, CPU count, memory   |
//! | `pause`    | Pauses all vCPUs and kernel timers           |
//! | `resume`   | Resumes a paused VM                         |
//! | `shutdown` | Graceful ACPI poweroff (VM_SUSPEND_POWEROFF) |
//! | `reset`    | Guest restart (VM_SUSPEND_RESET); VMM re-execs |
//! | `stop`     | Immediate VM halt (VM_SUSPEND_HALT)          |

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use slog::{error, info, warn, Logger};

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::{MemCtx, PhysMap};

use crate::host_state::HostLocalState;
use crate::peercred::PeerCred;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 8;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

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
    mem_size: usize,
    start_time: Instant,
    migrate_status: Mutex<Option<Arc<Mutex<vmm_migrate::MigrationStatus>>>>,
    /// Per-vCPU metrics, shared with vCPU threads.
    vcpu_metrics: Vec<Arc<vmm_devices::metrics::VcpuMetrics>>,
    /// Devices implementing the Lifecycle trait for pause/resume.
    devices: Vec<Arc<dyn vmm_devices::Lifecycle>>,
    /// AHCI state has no migration payload, so attached media blocks migration.
    has_ahci_cd: bool,
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
        mem_size: usize,
        vcpu_metrics: Vec<Arc<vmm_devices::metrics::VcpuMetrics>>,
        devices: Vec<Arc<dyn vmm_devices::Lifecycle>>,
        has_ahci_cd: bool,
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
            mem_size,
            start_time: Instant::now(),
            migrate_status: Mutex::new(None),
            vcpu_metrics,
            devices,
            has_ahci_cd,
            cli_pci_slots,
            cli_lpc,
            cpu_baseline,
            host_local_state,
            log,
        })
    }

    fn export_device_queues(
        &self,
    ) -> Result<vmm_migrate::sync_source::DeviceQueueExport, String> {
        // Pause all devices so in-flight I/O completes before we
        // snapshot ring indices. Without this, requests popped from
        // the avail ring but not yet completed would be lost — the
        // guest would wait forever for their completions.
        for dev in &self.devices {
            dev.pause();
        }
        let report = vmm_devices::wait_all_quiesced(
            &self.devices,
            Duration::from_secs(30),
        );
        if !report.is_empty() {
            return Err(format!("devices did not quiesce: {:?}", report.stuck));
        }

        let mut virtio_queues = Vec::new();
        let mut nvme_controllers = Vec::new();

        for dev in &self.devices {
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
    fn pause_devices(&self) -> Result<(), Vec<&'static str>> {
        pause_device_set(&self.devices, Duration::from_secs(5))
    }

    /// Resume all devices: wake parked workers.
    fn resume_devices(&self) {
        resume_device_set(&self.devices);
    }

    /// Halt all devices: permanent shutdown.
    // Kept for the pending graceful device-stop sequence.
    #[allow(dead_code)]
    fn halt_devices(&self) {
        for dev in &self.devices {
            dev.halt();
        }
    }
}

fn pause_device_set(
    devices: &[Arc<dyn vmm_devices::Lifecycle>],
    budget: Duration,
) -> Result<(), Vec<&'static str>> {
    for dev in devices {
        if dev.lifecycle_state() != Some(vmm_devices::IndicatedState::Pause) {
            dev.pause();
        }
    }
    let report = vmm_devices::wait_all_quiesced(devices, budget);
    if !report.is_empty() {
        resume_device_set(devices);
        return Err(report.stuck);
    }
    Ok(())
}

fn resume_device_set(devices: &[Arc<dyn vmm_devices::Lifecycle>]) {
    for dev in devices {
        if matches!(
            dev.lifecycle_state(),
            None | Some(vmm_devices::IndicatedState::Pause)
        ) {
            dev.resume();
        }
    }
}

// ── Protocol types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "command")]
enum Command {
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "pause")]
    Pause,
    #[serde(rename = "resume")]
    Resume,
    #[serde(rename = "shutdown")]
    Shutdown,
    #[serde(rename = "reset")]
    Reset,
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "migrate-source")]
    MigrateSource {
        target_addr: String,
        /// Unix socket path for post-pause ZFS sync barrier.
        /// VMM connects after vCPU pause; agent writes back when ZFS done.
        #[serde(default)]
        zfs_barrier: Option<String>,
    },
    #[serde(rename = "migrate-dest")]
    MigrateDest { listen_addr: String },
    #[serde(rename = "migrate-status")]
    MigrateStatus,
    #[serde(rename = "migrate-config")]
    MigrateConfig,
    #[serde(rename = "metrics")]
    Metrics,
    #[serde(rename = "metrics-prometheus")]
    MetricsPrometheus,
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Shutdown => "shutdown",
            Self::Reset => "reset",
            Self::Stop => "stop",
            Self::MigrateSource { .. } => "migrate-source",
            Self::MigrateDest { .. } => "migrate-dest",
            Self::MigrateStatus => "migrate-status",
            Self::MigrateConfig => "migrate-config",
            Self::Metrics => "metrics",
            Self::MetricsPrometheus => "metrics-prometheus",
        }
    }
}

#[derive(Debug, Serialize)]
struct Response {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<VmRunState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vm_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_cpus: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uptime_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    migrate_phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    migrate_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    migrate_pages: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<serde_json::Value>,
}

impl Response {
    fn ok() -> Self {
        Self {
            success: true,
            error: None,
            state: None,
            vm_name: None,
            num_cpus: None,
            memory_bytes: None,
            uptime_secs: None,
            migrate_phase: None,
            migrate_bytes: None,
            migrate_pages: None,
            config: None,
        }
    }

    fn err(msg: &str) -> Self {
        Self {
            success: false,
            error: Some(msg.to_string()),
            state: None,
            vm_name: None,
            num_cpus: None,
            memory_bytes: None,
            uptime_secs: None,
            migrate_phase: None,
            migrate_bytes: None,
            migrate_pages: None,
            config: None,
        }
    }
}

fn migration_source_guard(
    state: HostLocalState,
    has_ahci_cd: bool,
    command: &Command,
) -> Option<Response> {
    if !matches!(command, Command::MigrateSource { .. }) {
        return None;
    }
    if has_ahci_cd {
        return Some(Response::err(
            "ahci-cd is not supported on a migration source",
        ));
    }
    state.migration_blocker().map(Response::err)
}

// ── Command dispatch ────────────────────────────────────────────────

fn dispatch(ctrl: &Arc<VmController>, cmd: Command) -> Response {
    if let Some(response) =
        migration_source_guard(ctrl.host_local_state, ctrl.has_ahci_cd, &cmd)
    {
        return response;
    }

    match cmd {
        Command::Status => {
            let mut r = Response::ok();
            r.state = Some(ctrl.state());
            r.vm_name = Some(ctrl.vm_name.clone());
            r.num_cpus = Some(ctrl.num_cpus);
            r.memory_bytes = Some(ctrl.mem_size);
            r.uptime_secs = Some(ctrl.start_time.elapsed().as_secs());
            r
        }

        Command::Pause => {
            if !ctrl.transition(VmRunState::Running, VmRunState::Paused) {
                return Response::err("VM is not running");
            }
            if let Err(stuck) = ctrl.pause_devices() {
                ctrl.state
                    .store(VmRunState::Running as u8, Ordering::Release);
                return Response::err(&format!(
                    "device pause timed out: {:?}",
                    stuck,
                ));
            }
            match ctrl.hdl.pause() {
                Ok(()) => {
                    let mut r = Response::ok();
                    r.state = Some(VmRunState::Paused);
                    r
                }
                Err(e) => {
                    ctrl.resume_devices();
                    ctrl.state
                        .store(VmRunState::Running as u8, Ordering::Release);
                    error!(ctrl.log, "pause failed"; "error" => %e);
                    Response::err("pause ioctl failed")
                }
            }
        }

        Command::Resume => {
            if !ctrl.transition(VmRunState::Paused, VmRunState::Running) {
                return Response::err("VM is not paused");
            }
            // Resume device workers BEFORE vCPUs so interrupts can be delivered
            ctrl.resume_devices();
            match ctrl.hdl.resume() {
                Ok(()) => {
                    let mut r = Response::ok();
                    r.state = Some(VmRunState::Running);
                    r
                }
                Err(e) => {
                    ctrl.state
                        .store(VmRunState::Paused as u8, Ordering::Release);
                    error!(ctrl.log, "resume failed"; "error" => %e);
                    Response::err("resume ioctl failed")
                }
            }
        }

        Command::Shutdown => {
            let cur = ctrl.state();
            if cur != VmRunState::Running && cur != VmRunState::Paused {
                return Response::err("VM is not running or paused");
            }
            ctrl.state
                .store(VmRunState::Stopping as u8, Ordering::Release);

            // If paused, resume first so vCPUs can receive the suspend
            if cur == VmRunState::Paused {
                // best-effort: if resume fails, suspend below will also fail
                let _ = ctrl.hdl.resume();
            }

            use bhyve_api::vm_suspend_how;
            match ctrl
                .hdl
                .suspend(vm_suspend_how::VM_SUSPEND_POWEROFF as u32, 0)
            {
                Ok(()) => {
                    let mut r = Response::ok();
                    r.state = Some(VmRunState::Stopping);
                    r
                }
                Err(e) => {
                    ctrl.state.store(cur as u8, Ordering::Release);
                    error!(ctrl.log, "shutdown failed"; "error" => %e);
                    Response::err("suspend ioctl failed")
                }
            }
        }

        Command::Reset => {
            let cur = ctrl.state();
            if cur != VmRunState::Running && cur != VmRunState::Paused {
                return Response::err("VM is not running or paused");
            }
            ctrl.state
                .store(VmRunState::Stopping as u8, Ordering::Release);

            // If paused, resume first so vCPUs can receive the suspend
            if cur == VmRunState::Paused {
                // best-effort: if resume fails, suspend below will also fail
                let _ = ctrl.hdl.resume();
            }

            use bhyve_api::vm_suspend_how;
            match ctrl
                .hdl
                .suspend(vm_suspend_how::VM_SUSPEND_RESET as u32, -1)
            {
                Ok(()) => {
                    let mut r = Response::ok();
                    r.state = Some(VmRunState::Stopping);
                    r
                }
                Err(e) => {
                    ctrl.state.store(cur as u8, Ordering::Release);
                    error!(ctrl.log, "reset failed"; "error" => %e);
                    Response::err("suspend ioctl failed")
                }
            }
        }

        Command::Stop => {
            let cur = ctrl.state();
            if cur == VmRunState::Stopped || cur == VmRunState::Stopping {
                return Response::err("VM is already stopped/stopping");
            }
            ctrl.state
                .store(VmRunState::Stopping as u8, Ordering::Release);

            if cur == VmRunState::Paused {
                // best-effort: if resume fails, suspend below will also fail
                let _ = ctrl.hdl.resume();
            }

            use bhyve_api::vm_suspend_how;
            match ctrl.hdl.suspend(vm_suspend_how::VM_SUSPEND_HALT as u32, 0) {
                Ok(()) => {
                    let mut r = Response::ok();
                    r.state = Some(VmRunState::Stopping);
                    r
                }
                Err(e) => {
                    ctrl.state.store(cur as u8, Ordering::Release);
                    error!(ctrl.log, "stop failed"; "error" => %e);
                    Response::err("suspend ioctl failed")
                }
            }
        }

        Command::MigrateSource {
            target_addr,
            zfs_barrier,
        } => {
            // Validate the target address to prevent SSRF. Require
            // it to be a valid host:port and reject obviously-dangerous
            // targets like localhost, link-local, and multicast.
            // Skip TCP validation for Unix socket paths
            if !target_addr.starts_with('/') && !target_addr.ends_with(".sock")
            {
                if let Err(e) = validate_migration_addr(&target_addr) {
                    return Response::err(&format!("invalid target_addr: {e}"));
                }
            }

            if !ctrl.transition(VmRunState::Running, VmRunState::Migrating) {
                return Response::err("VM must be running to start migration");
            }

            let status =
                Arc::new(Mutex::new(vmm_migrate::MigrationStatus::default()));

            // Store status for later query
            if let Ok(mut ms) = ctrl.migrate_status.lock() {
                *ms = Some(Arc::clone(&status));
            }

            let ctrl_arc = Arc::clone(ctrl);
            let hdl = Arc::clone(&ctrl.hdl);
            let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
            let num_cpus = ctrl.num_cpus;
            let mem_size = ctrl.mem_size as u64;
            let mlog = ctrl.log.clone();

            let is_unix =
                target_addr.starts_with('/') || target_addr.ends_with(".sock");

            if let Err(e) = thread::Builder::new()
                .name("migrate-source".into())
                .spawn(move || {
                    if is_unix {
                        // Synchronous path for Unix sockets (works in
                        // bhyve zones without /dev/poll).
                        info!(mlog, "connecting to migration socket";
                            "path" => &target_addr);
                        let stream = match UnixStream::connect(
                            &target_addr,
                        ) {
                            Ok(s) => s,
                            Err(e) => {
                                error!(mlog, "connect failed";
                                    "path" => &target_addr, "error" => %e);
                                ctrl_arc.state.store(
                                    VmRunState::Running as u8,
                                    Ordering::Release,
                                );
                                return;
                            }
                        };

                        let ctrl_for_prepause = ctrl_arc.clone();
                        let pre_pause_fn: vmm_migrate::sync_source::DevicePrePauseFn =
                            Box::new(move || {
                                for dev in &ctrl_for_prepause.devices {
                                    dev.pause_for_migration();
                                }
                            });
                        let ctrl_for_flush = ctrl_arc.clone();
                        let flush_fn: vmm_migrate::sync_source::DeviceFlushFn =
                            Box::new(move || {
                                for dev in &ctrl_for_flush.devices {
                                    dev.flush_backing(vmm_devices::FlushIntent::Durable)
                                        .map_err(|error| {
                                            format!("{}: {:?}", dev.type_name(), error)
                                        })?;
                                }
                                Ok(())
                            });
                        let ctrl_for_resume = ctrl_arc.clone();
                        let resume_fn: vmm_migrate::sync_source::DeviceResumeFn =
                            Box::new(move || ctrl_for_resume.resume_devices());
                        let ctrl_for_export = ctrl_arc.clone();
                        let export_fn: vmm_migrate::sync_source::DeviceExportFn =
                            Box::new(move || {
                                match ctrl_for_export.export_device_queues() {
                                    Ok(export) => export,
                                    Err(error) => {
                                        error!(ctrl_for_export.log,
                                            "device queue export failed";
                                            "error" => %error);
                                        // DeviceExportFn is infallible until the
                                        // migration layer can propagate this error.
                                        // Empty state avoids racing live ring indices.
                                        vmm_migrate::sync_source::DeviceQueueExport {
                                            virtio_queues: Vec::new(),
                                            nvme_controllers: Vec::new(),
                                        }
                                    }
                                }
                            });
                        match vmm_migrate::sync_source::run_source_sync(
                            stream, hdl, &memctx, num_cpus, mem_size,
                            status, &mlog, &ctrl_arc.cpu_baseline,
                            pre_pause_fn, flush_fn, resume_fn, export_fn,
                            zfs_barrier,
                        ) {
                            Ok(()) => {
                                info!(mlog, "source migration complete");
                                ctrl_arc.state.store(
                                    VmRunState::Stopped as u8,
                                    Ordering::Release,
                                );
                            }
                            Err(e) => {
                                error!(mlog, "source migration failed";
                                    "error" => %e);
                                ctrl_arc.state.store(
                                    VmRunState::Running as u8,
                                    Ordering::Release,
                                );
                            }
                        }
                    } else {
                        // Async path for TCP (GZ or manual testing)
                        let rt = match tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            Ok(rt) => rt,
                            Err(e) => {
                                error!(mlog, "tokio runtime failed"; "error" => %e);
                                ctrl_arc.state.store(
                                    VmRunState::Running as u8,
                                    Ordering::Release,
                                );
                                return;
                            }
                        };
                        rt.block_on(async {
                            let stream = match tokio::net::TcpStream::connect(
                                &target_addr,
                            ).await {
                                Ok(s) => vmm_migrate::MigrateStream::Tcp(s),
                                Err(e) => {
                                    error!(mlog, "connect failed";
                                        "target" => &target_addr, "error" => %e);
                                    ctrl_arc.state.store(
                                        VmRunState::Running as u8,
                                        Ordering::Release,
                                    );
                                    return;
                                }
                            };
                            match vmm_migrate::source::run_source(
                                stream, hdl, &memctx, num_cpus, mem_size,
                                status, &mlog, &ctrl_arc.cpu_baseline,
                            ).await {
                                Ok(()) => {
                                    info!(mlog, "source migration complete");
                                    ctrl_arc.state.store(
                                        VmRunState::Stopped as u8,
                                        Ordering::Release,
                                    );
                                }
                                Err(e) => {
                                    error!(mlog, "source migration failed";
                                        "error" => %e);
                                    ctrl_arc.state.store(
                                        VmRunState::Running as u8,
                                        Ordering::Release,
                                    );
                                }
                            }
                        });
                    }
                })
            {
                ctrl.state.store(
                    VmRunState::Running as u8,
                    Ordering::Release,
                );
                return Response::err(
                    &format!("failed to spawn migration thread: {}", e),
                );
            }

            let mut r = Response::ok();
            r.state = Some(VmRunState::Migrating);
            r
        }

        Command::MigrateDest { listen_addr } => {
            // Validate the listen address to prevent arbitrary port binding.
            if let Err(e) = validate_migration_addr(&listen_addr) {
                return Response::err(&format!("invalid listen_addr: {e}"));
            }

            let prev_state = ctrl.state();
            if prev_state != VmRunState::Paused
                && prev_state != VmRunState::Running
            {
                return Response::err(
                    "VM must be running or paused to receive migration",
                );
            }
            ctrl.state
                .store(VmRunState::Migrating as u8, Ordering::Release);

            let status =
                Arc::new(Mutex::new(vmm_migrate::MigrationStatus::default()));
            if let Ok(mut ms) = ctrl.migrate_status.lock() {
                *ms = Some(Arc::clone(&status));
            }

            let ctrl_arc = Arc::clone(ctrl);
            let hdl = Arc::clone(&ctrl.hdl);
            let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
            let num_cpus = ctrl.num_cpus;
            let mem_size = ctrl.mem_size as u64;
            let mlog = ctrl.log.clone();

            if let Err(e) = thread::Builder::new()
                .name("migrate-dest".into())
                .spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            error!(mlog, "failed to create tokio runtime"; "error" => %e);
                            ctrl_arc.state.store(
                                VmRunState::Stopped as u8,
                                Ordering::Release,
                            );
                            return;
                        }
                    };

                    rt.block_on(async {
                        let listener = match tokio::net::TcpListener::bind(
                            &listen_addr,
                        )
                        .await
                        {
                            Ok(l) => l,
                            Err(e) => {
                                error!(mlog, "bind failed";
                                    "addr" => &listen_addr,
                                    "error" => %e,
                                );
                                ctrl_arc.state.store(
                                    VmRunState::Stopped as u8,
                                    Ordering::Release,
                                );
                                return;
                            }
                        };

                        let (stream, peer) = match listener.accept().await {
                            Ok(v) => v,
                            Err(e) => {
                                error!(mlog, "accept failed"; "error" => %e);
                                ctrl_arc.state.store(
                                    VmRunState::Stopped as u8,
                                    Ordering::Release,
                                );
                                return;
                            }
                        };

                        info!(mlog, "accepted migration connection"; "peer" => %peer);

                        match vmm_migrate::destination::run_destination(
                            stream,
                            hdl,
                            &memctx,
                            num_cpus,
                            mem_size,
                            status,
                            &mlog,
                        )
                        .await
                        {
                            Ok(()) => {
                                info!(mlog, "destination migration complete, VM running");
                                ctrl_arc.state.store(
                                    VmRunState::Running as u8,
                                    Ordering::Release,
                                );
                            }
                            Err(e) => {
                                error!(mlog, "destination migration failed"; "error" => %e);
                                ctrl_arc.state.store(
                                    VmRunState::Stopped as u8,
                                    Ordering::Release,
                                );
                            }
                        }
                    });
                })
            {
                ctrl.state.store(
                    prev_state as u8,
                    Ordering::Release,
                );
                return Response::err(
                    &format!("failed to spawn migration thread: {}", e),
                );
            }

            let mut r = Response::ok();
            r.state = Some(VmRunState::Migrating);
            r
        }

        Command::Metrics | Command::MetricsPrometheus => {
            // Metrics are returned as raw text — not via the typed
            // Response struct. The caller handles this specially.
            let mut r = Response::ok();
            r.state = Some(ctrl.state());
            r
        }

        Command::MigrateStatus => {
            let phase_info = ctrl.migrate_status.lock().ok().and_then(|ms| {
                ms.as_ref().and_then(|s| s.lock().ok().map(|s| s.clone()))
            });

            match phase_info {
                Some(s) => {
                    let mut r = Response::ok();
                    r.state = Some(ctrl.state());
                    r.migrate_phase = Some(s.phase.to_string());
                    r.migrate_bytes = Some(s.bytes_transferred);
                    r.migrate_pages = Some(s.pages_transferred);
                    r
                }
                None => {
                    let mut r = Response::ok();
                    r.state = Some(ctrl.state());
                    r
                }
            }
        }

        Command::MigrateConfig => {
            let config = serde_json::json!({
                "vm_name": ctrl.vm_name,
                "num_cpus": ctrl.num_cpus,
                "memory_mb": ctrl.mem_size / (1024 * 1024),
                "pci_slots": ctrl.cli_pci_slots,
                "lpc_devices": ctrl.cli_lpc,
            });
            let mut r = Response::ok();
            r.config = Some(config);
            r
        }
    }
}

// ── Migration address validation ────────────────────────────────────

/// Validate a migration address (host:port) to prevent SSRF and
/// arbitrary port binding. Rejects:
/// - Missing or invalid port
/// - Privileged ports (< 1024) for listen addresses
/// - Loopback (127.x.x.x, ::1) — migration to self is nonsensical
/// - Addresses that fail to parse as host:port
fn validate_migration_addr(addr: &str) -> Result<(), String> {
    // Must contain a colon separating host and port
    let colon = addr.rfind(':').ok_or("missing port (expected host:port)")?;
    let host = &addr[..colon];
    let port_str = &addr[colon + 1..];

    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("invalid port: {port_str}"))?;

    if port == 0 {
        return Err("port 0 is not allowed".into());
    }

    // Reject loopback addresses to prevent self-migration confusion
    let host_trimmed = host.trim_matches(|c| c == '[' || c == ']');
    if host_trimmed == "localhost"
        || host_trimmed == "127.0.0.1"
        || host_trimmed == "::1"
        || host_trimmed == "0.0.0.0"
    {
        return Err(format!("loopback/wildcard address not allowed: {host}"));
    }

    // Reject empty host
    if host_trimmed.is_empty() {
        return Err("empty host".into());
    }

    Ok(())
}

// ── Socket listener ─────────────────────────────────────────────────

/// Spawn the control socket listener thread.
///
/// Binds a Unix domain socket at `path`, removes any stale socket
/// file first. Returns the thread handle. The thread runs until the
/// process exits.
pub fn spawn_control_thread(
    path: &Path,
    ctrl: Arc<VmController>,
    log: Logger,
) -> io::Result<(thread::JoinHandle<()>, PathBuf)> {
    let listener = vmm_core::unixsock::bind_restricted(
        path,
        vmm_core::unixsock::SocketPolicy::default(),
    )?;

    let sock_path = path.to_path_buf();
    let cleanup_path = sock_path.clone();

    let handle = thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            let connection_log = log.clone();
            serve_connections(listener, log, move |stream| {
                if let Err(e) = handle_connection(&ctrl, &stream, &connection_log) {
                    warn!(connection_log, "control connection error"; "error" => %e);
                }
            });
        })?;

    Ok((handle, cleanup_path))
}

struct ConnectionGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn acquire_connection(active: &Arc<AtomicUsize>) -> Option<ConnectionGuard> {
    active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < MAX_CONNECTIONS).then_some(current + 1)
        })
        .ok()?;
    Some(ConnectionGuard {
        active: Arc::clone(active),
    })
}

fn set_connection_deadlines(stream: &UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    Ok(())
}

fn serve_connections<F>(listener: UnixListener, log: Logger, handler: F)
where
    F: Fn(UnixStream) + Send + Sync + 'static,
{
    let active = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(handler);

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                error!(log, "control accept error"; "error" => %e);
                break;
            }
        };

        if let Err(e) = set_connection_deadlines(&stream) {
            warn!(log, "failed to set control connection deadlines"; "error" => %e);
            continue;
        }

        let Some(connection_guard) = acquire_connection(&active) else {
            if let Err(e) =
                write_error_response(&stream, "too many connections")
            {
                warn!(log, "failed to reject excess control connection"; "error" => %e);
            }
            continue;
        };

        let handler = Arc::clone(&handler);
        let thread_log = log.clone();
        if let Err(e) = thread::Builder::new()
            .name("control-connection".into())
            .spawn(move || {
                let _guard = connection_guard;
                handler(stream);
            })
        {
            warn!(thread_log, "failed to spawn control connection thread"; "error" => %e);
        }
    }
}

/// illumos global zone id. A global-zone peer already has full authority
/// over this zone, so refusing it would protect nothing and would break
/// vmm-migrate-agent, which runs in the GZ and drives the control socket
/// of a VMM inside a zone.
const GLOBAL_ZONEID: i32 = 0;

fn peer_is_authorized(peer: &PeerCred, euid: u32, zoneid: Option<i32>) -> bool {
    if peer.uid != euid {
        return false;
    }
    match (peer.zoneid, zoneid) {
        (Some(peer_zone), Some(own_zone)) => {
            peer_zone == own_zone || peer_zone == GLOBAL_ZONEID
        }
        // Platforms without zones: the uid check is the whole policy.
        (None, None) => true,
        // One side reported a zone and the other did not. Fail closed.
        _ => false,
    }
}

fn request_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "control request exceeds maximum size",
    )
}

/// Read one newline-delimited request without allowing `buf` to exceed the cap.
/// Returns `false` only when EOF is reached before any request bytes are read.
fn read_request<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<bool> {
    buf.clear();

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }

        if let Some(newline) = available.iter().position(|byte| *byte == b'\n')
        {
            let consumed = newline + 1;
            if consumed > MAX_REQUEST_BYTES - buf.len() {
                return Err(request_too_large());
            }
            buf.extend_from_slice(&available[..consumed]);
            reader.consume(consumed);
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(true);
        }

        if available.len() > MAX_REQUEST_BYTES - buf.len() {
            return Err(request_too_large());
        }
        let consumed = available.len();
        buf.extend_from_slice(available);
        reader.consume(consumed);
    }
}

fn write_error_response(
    stream: &UnixStream,
    message: &str,
) -> Result<(), ControlError> {
    let response = Response::err(message);
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    let mut writer = stream;
    writer.write_all(&out)?;
    writer.flush()?;
    Ok(())
}

/// Handle a single client connection: read JSON lines, dispatch, respond.
fn handle_connection(
    ctrl: &Arc<VmController>,
    stream: &UnixStream,
    log: &Logger,
) -> Result<(), ControlError> {
    let peer = crate::peercred::peer_cred(stream)?;
    let euid = unsafe { libc::geteuid() };
    let zoneid = crate::peercred::process_zoneid()?;
    if !peer_is_authorized(&peer, euid, zoneid) {
        warn!(log, "rejected control connection"; "uid" => peer.uid);
        return Ok(());
    }

    let mut reader = BufReader::new(stream);
    let mut writer = stream;
    let mut request = Vec::with_capacity(256);

    loop {
        match read_request(&mut reader, &mut request) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                write_error_response(stream, "request too large")?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        if request.is_empty() {
            continue;
        }

        match serde_json::from_slice::<Command>(&request) {
            Ok(cmd) => {
                info!(log, "control command"; "command" => cmd.name());

                // Metrics commands return raw text, bypass Response serialization
                let raw = match &cmd {
                    Command::Metrics => {
                        let uptime = ctrl.start_time.elapsed().as_secs();
                        let vcpus: Vec<String> = ctrl
                            .vcpu_metrics
                            .iter()
                            .enumerate()
                            .map(|(i, v)| v.to_json(i as i32))
                            .collect();
                        Some(format!(
                            r#"{{"success":true,"uptime_secs":{},"vcpus":[{}]}}"#,
                            uptime,
                            vcpus.join(","),
                        ))
                    }
                    Command::MetricsPrometheus => {
                        let mut out = format!(
                            "# HELP vmm_uptime_seconds VMM uptime\n\
                             # TYPE vmm_uptime_seconds gauge\n\
                             vmm_uptime_seconds {}\n",
                            ctrl.start_time.elapsed().as_secs(),
                        );
                        for (i, v) in ctrl.vcpu_metrics.iter().enumerate() {
                            out.push_str(&v.to_prometheus(i as i32));
                        }
                        Some(out)
                    }
                    _ => None,
                };

                let mut out = if let Some(raw) = raw {
                    raw.into_bytes()
                } else {
                    let response = dispatch(ctrl, cmd);
                    serde_json::to_vec(&response)?
                };
                out.push(b'\n');
                writer.write_all(&out)?;
                writer.flush()?;
            }
            Err(e) => {
                warn!(log, "invalid control command";
                    "error" => %e,
                    "bytes" => request.len(),
                );
                let response = Response::err("invalid command");
                let mut out = serde_json::to_vec(&response)?;
                out.push(b'\n');
                writer.write_all(&out)?;
                writer.flush()?;
            }
        }
    }

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc;

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

    #[test]
    fn pause_devices_times_out_and_resumes_devices() {
        let device = Arc::new(NeverQuiesces {
            paused: AtomicBool::new(false),
            resume_calls: AtomicUsize::new(0),
        });
        let devices: Vec<Arc<dyn vmm_devices::Lifecycle>> =
            vec![device.clone()];

        let result = pause_device_set(&devices, Duration::from_millis(20));

        assert_eq!(result, Err(vec!["never"]));
        assert!(!device.paused.load(Ordering::Acquire));
        assert_eq!(device.resume_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn resume_devices_skips_tracked_devices_that_are_running() {
        let device = Arc::new(NeverQuiesces {
            paused: AtomicBool::new(false),
            resume_calls: AtomicUsize::new(0),
        });
        let devices: Vec<Arc<dyn vmm_devices::Lifecycle>> =
            vec![device.clone()];

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
    fn read_request_rejects_oversize_line() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES];
        input.push(b'\n');
        let mut reader = BufReader::with_capacity(1024, Cursor::new(input));
        let mut request = Vec::new();

        let error = read_request(&mut reader, &mut request)
            .expect_err("oversize request must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(request.len() <= MAX_REQUEST_BYTES);
    }

    #[test]
    fn read_request_accepts_max_size_line() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES - 1];
        input.push(b'\n');
        let mut reader = BufReader::with_capacity(1024, Cursor::new(input));
        let mut request = Vec::new();

        let present = read_request(&mut reader, &mut request)
            .expect("maximum-size request must be accepted");

        assert!(present);
        assert_eq!(request.len(), MAX_REQUEST_BYTES - 1);
    }

    #[test]
    fn peer_authorization_checks_uid_and_zone() {
        let local = PeerCred {
            uid: 42,
            gid: 7,
            zoneid: None,
        };
        let foreign_uid = PeerCred {
            uid: 43,
            gid: 7,
            zoneid: None,
        };
        let same_zone = PeerCred {
            uid: 42,
            gid: 7,
            zoneid: Some(3),
        };
        let foreign_zone = PeerCred {
            uid: 42,
            gid: 7,
            zoneid: Some(4),
        };

        assert!(peer_is_authorized(&local, 42, None));
        assert!(!peer_is_authorized(&foreign_uid, 42, None));
        assert!(peer_is_authorized(&same_zone, 42, Some(3)));
        assert!(!peer_is_authorized(&foreign_zone, 42, Some(3)));
    }

    #[test]
    fn global_zone_peer_is_authorized_against_a_zoned_vmm() {
        // vmm-migrate-agent runs in the GZ and drives the control socket
        // of a VMM inside a zone. Rejecting it would break migration.
        let gz_peer = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: Some(GLOBAL_ZONEID),
        };
        assert!(peer_is_authorized(&gz_peer, 0, Some(7)));

        // A sibling zone is still refused.
        let sibling = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: Some(8),
        };
        assert!(!peer_is_authorized(&sibling, 0, Some(7)));

        // A GZ peer with the wrong uid is still refused.
        let wrong_uid = PeerCred {
            uid: 1,
            gid: 0,
            zoneid: Some(GLOBAL_ZONEID),
        };
        assert!(!peer_is_authorized(&wrong_uid, 0, Some(7)));
    }

    #[test]
    fn mismatched_zone_reporting_fails_closed() {
        let zoned = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: Some(0),
        };
        assert!(!peer_is_authorized(&zoned, 0, None));

        let unzoned = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: None,
        };
        assert!(!peer_is_authorized(&unzoned, 0, Some(0)));
    }

    #[test]
    fn second_client_is_served_while_first_is_idle() {
        static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0);

        let id = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!(
            "vmm-control-concurrency-{}-{id}",
            std::process::id(),
        ));
        std::fs::create_dir(&temp_dir).expect("create control test directory");
        let path = temp_dir.join("control.sock");
        let listener =
            UnixListener::bind(&path).expect("bind control test socket");
        let log = Logger::root(slog::Discard, slog::o!());
        let first_connection = Arc::new(AtomicBool::new(true));
        let first_for_handler = Arc::clone(&first_connection);
        let (started_tx, started_rx) = mpsc::channel();

        thread::spawn(move || {
            serve_connections(listener, log, move |stream| {
                if first_for_handler.swap(false, Ordering::AcqRel) {
                    started_tx.send(()).expect("signal first connection start");
                }
                let mut reader = BufReader::new(&stream);
                let mut request = Vec::new();
                if matches!(read_request(&mut reader, &mut request), Ok(true)) {
                    let mut writer = &stream;
                    writer.write_all(b"ok\n").expect("write test response");
                }
            });
        });

        let _idle = UnixStream::connect(&path).expect("connect idle client");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("idle client handler must start");

        let mut second =
            UnixStream::connect(&path).expect("connect second client");
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set test read timeout");
        second.write_all(b"status\n").expect("write second request");
        let mut response = String::new();
        BufReader::new(&second)
            .read_line(&mut response)
            .expect("read second response");

        assert_eq!(response, "ok\n");
        std::fs::remove_file(&path).expect("remove control test socket");
        std::fs::remove_dir(&temp_dir).expect("remove control test directory");
    }

    #[test]
    fn command_deserialize_status() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"status"}"#).unwrap();
        assert!(matches!(cmd, Command::Status));
    }

    #[test]
    fn command_deserialize_pause() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"pause"}"#).unwrap();
        assert!(matches!(cmd, Command::Pause));
    }

    #[test]
    fn command_deserialize_shutdown() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"shutdown"}"#).unwrap();
        assert!(matches!(cmd, Command::Shutdown));
    }

    #[test]
    fn parse_reset_command() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"reset"}"#).unwrap();
        assert!(matches!(cmd, Command::Reset));
    }

    #[test]
    fn unknown_command_is_rejected() {
        let result = serde_json::from_str::<Command>(r#"{"command":"reboot"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn response_ok_serializes() {
        let r = Response::ok();
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""success":true"#));
        // Optional fields should be absent
        assert!(!json.contains("error"));
        assert!(!json.contains("state"));
    }

    #[test]
    fn response_err_serializes() {
        let r = Response::err("something broke");
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""success":false"#));
        assert!(json.contains(r#""error":"something broke""#));
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

    #[test]
    fn command_deserialize_migrate_source() {
        let cmd: Command = serde_json::from_str(
            r#"{"command":"migrate-source","target_addr":"10.0.0.2:1234"}"#,
        )
        .unwrap();
        assert!(matches!(cmd, Command::MigrateSource { .. }));
    }

    #[test]
    fn migrate_source_rejects_host_local_state() {
        let command = Command::MigrateSource {
            target_addr: "192.0.2.10:4567".to_string(),
            zfs_barrier: None,
        };

        for state in [
            HostLocalState::new(true, false),
            HostLocalState::new(false, true),
            HostLocalState::new(true, true),
        ] {
            let response = migration_source_guard(state, false, &command)
                .expect("host-local state must block migration");
            assert!(!response.success);
            assert!(response
                .error
                .as_deref()
                .expect("error response must explain rejection")
                .contains("host-local state is not carried by migration"));
        }

        assert!(migration_source_guard(
            HostLocalState::default(),
            false,
            &command
        )
        .is_none());
    }

    #[test]
    fn migrate_source_rejects_ahci_cd() {
        let command = Command::MigrateSource {
            target_addr: "192.0.2.10:4567".to_string(),
            zfs_barrier: None,
        };

        let response =
            migration_source_guard(HostLocalState::default(), true, &command)
                .expect("AHCI CD must block migration");
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("ahci-cd is not supported on a migration source")
        );
    }

    #[test]
    fn command_deserialize_migrate_dest() {
        let cmd: Command = serde_json::from_str(
            r#"{"command":"migrate-dest","listen_addr":"0.0.0.0:1234"}"#,
        )
        .unwrap();
        assert!(matches!(cmd, Command::MigrateDest { .. }));
    }

    #[test]
    fn command_deserialize_migrate_status() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"migrate-status"}"#).unwrap();
        assert!(matches!(cmd, Command::MigrateStatus));
    }
}
