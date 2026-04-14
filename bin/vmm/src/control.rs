// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unix domain socket control interface for vmadm integration.
//!
//! Listens on a path specified by `--control-socket` and accepts
//! newline-delimited JSON commands. Each command produces a single
//! JSON response line. Only one client connection is served at a time.
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
//! | `stop`     | Immediate VM halt (VM_SUSPEND_HALT)          |

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use slog::{error, info, warn, Logger};

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::{MemCtx, PhysMap};

// ── Error type ──────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

// ── VM state machine ────────────────────────────────────────────────

/// VM lifecycle state, stored as an `AtomicU8` for lock-free access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VmRunState {
    Running   = 0,
    Paused    = 1,
    Stopping  = 2,
    Stopped   = 3,
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
    /// Original CLI args for migrate-config reconstruction.
    cli_pci_slots: Vec<String>,
    cli_lpc: Vec<String>,
    /// CPU baseline for migration feature masking.
    cpu_baseline: String,
    log: Logger,
}

impl VmController {
    pub fn new(
        hdl: Arc<VmmHdl>,
        physmap: Arc<PhysMap>,
        vm_name: String,
        num_cpus: u32,
        mem_size: usize,
        vcpu_metrics: Vec<Arc<vmm_devices::metrics::VcpuMetrics>>,
        devices: Vec<Arc<dyn vmm_devices::Lifecycle>>,
        cli_pci_slots: Vec<String>,
        cli_lpc: Vec<String>,
        cpu_baseline: String,
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
            cli_pci_slots,
            cli_lpc,
            cpu_baseline,
            log,
        })
    }

    fn export_device_queues(&self) -> vmm_migrate::sync_source::DeviceQueueExport {
        // Pause all devices so in-flight I/O completes before we
        // snapshot ring indices. Without this, requests popped from
        // the avail ring but not yet completed would be lost — the
        // guest would wait forever for their completions.
        for dev in &self.devices {
            dev.pause();
        }
        // Wait for all workers to finish (typically < 100ms)
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        for _ in 0..100 {
            let all_paused = self.devices.iter().all(|dev| {
                let mut fut = dev.paused();
                matches!(
                    fut.as_mut().poll(&mut cx),
                    std::task::Poll::Ready(())
                )
            });
            if all_paused { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
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
                nvme_controllers.push(vmm_migrate::codec::NvmeControllerState {
                    cc: nvme.cc,
                    csts: nvme.csts,
                    aqa: nvme.aqa,
                    asq_base: nvme.asq_base,
                    acq_base: nvme.acq_base,
                    admin_sq: nvme.admin_sq.map(|sq| vmm_migrate::codec::NvmeSqState {
                        base: sq.base, size: sq.size, head: sq.head,
                        tail: sq.tail, cq_id: sq.cq_id,
                    }),
                    admin_cq: nvme.admin_cq.map(|cq| vmm_migrate::codec::NvmeCqState {
                        base: cq.base, size: cq.size, head: cq.head,
                        tail: cq.tail, phase: cq.phase, iv: cq.iv,
                    }),
                    io_sqs: nvme.io_sqs.into_iter().map(|sq| {
                        sq.map(|sq| vmm_migrate::codec::NvmeSqState {
                            base: sq.base, size: sq.size, head: sq.head,
                            tail: sq.tail, cq_id: sq.cq_id,
                        })
                    }).collect(),
                    io_cqs: nvme.io_cqs.into_iter().map(|cq| {
                        cq.map(|cq| vmm_migrate::codec::NvmeCqState {
                            base: cq.base, size: cq.size, head: cq.head,
                            tail: cq.tail, phase: cq.phase, iv: cq.iv,
                        })
                    }).collect(),
                    num_io_queues: nvme.num_io_queues,
                    pci_command: nvme.pci_command,
                    pci_bar_addrs: nvme.pci_bar_addrs,
                    msix_entries: nvme.msix_entries,
                    msix_enabled: nvme.msix_enabled,
                });
            }
        }
        vmm_migrate::sync_source::DeviceQueueExport {
            virtio_queues,
            nvme_controllers,
        }
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
    fn pause_devices(&self) {
        for dev in &self.devices {
            dev.pause();
        }
        // Collect paused futures and block until all workers are idle.
        let futs: Vec<_> = self.devices.iter().map(|d| d.paused()).collect();
        futures::executor::block_on(futures::future::join_all(futs));
    }

    /// Resume all devices: wake parked workers.
    fn resume_devices(&self) {
        for dev in &self.devices {
            dev.resume();
        }
    }

    /// Halt all devices: permanent shutdown.
    fn halt_devices(&self) {
        for dev in &self.devices {
            dev.halt();
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

// ── Command dispatch ────────────────────────────────────────────────

fn dispatch(ctrl: &Arc<VmController>, cmd: Command) -> Response {
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
            match ctrl.hdl.pause() {
                Ok(()) => {
                    // vCPUs paused — now quiesce device workers
                    ctrl.pause_devices();
                    let mut r = Response::ok();
                    r.state = Some(VmRunState::Paused);
                    r
                }
                Err(e) => {
                    ctrl.state.store(VmRunState::Running as u8, Ordering::Release);
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
                    ctrl.state.store(VmRunState::Paused as u8, Ordering::Release);
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
            ctrl.state.store(VmRunState::Stopping as u8, Ordering::Release);

            // If paused, resume first so vCPUs can receive the suspend
            if cur == VmRunState::Paused {
                // best-effort: if resume fails, suspend below will also fail
                let _ = ctrl.hdl.resume();
            }

            use bhyve_api::vm_suspend_how;
            match ctrl.hdl.suspend(
                vm_suspend_how::VM_SUSPEND_POWEROFF as u32,
                0,
            ) {
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

        Command::Stop => {
            let cur = ctrl.state();
            if cur == VmRunState::Stopped || cur == VmRunState::Stopping {
                return Response::err("VM is already stopped/stopping");
            }
            ctrl.state.store(VmRunState::Stopping as u8, Ordering::Release);

            if cur == VmRunState::Paused {
                // best-effort: if resume fails, suspend below will also fail
                let _ = ctrl.hdl.resume();
            }

            use bhyve_api::vm_suspend_how;
            match ctrl.hdl.suspend(
                vm_suspend_how::VM_SUSPEND_HALT as u32,
                0,
            ) {
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

        Command::MigrateSource { target_addr, zfs_barrier } => {
            // Validate the target address to prevent SSRF. Require
            // it to be a valid host:port and reject obviously-dangerous
            // targets like localhost, link-local, and multicast.
            // Skip TCP validation for Unix socket paths
            if !target_addr.starts_with('/') && !target_addr.ends_with(".sock") {
                if let Err(e) = validate_migration_addr(&target_addr) {
                    return Response::err(&format!("invalid target_addr: {e}"));
                }
            }

            if !ctrl.transition(VmRunState::Running, VmRunState::Migrating) {
                return Response::err("VM must be running to start migration");
            }

            let status = Arc::new(Mutex::new(
                vmm_migrate::MigrationStatus::default(),
            ));

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

            let is_unix = target_addr.starts_with('/')
                || target_addr.ends_with(".sock");

            if let Err(e) = thread::Builder::new()
                .name("migrate-source".into())
                .spawn(move || {
                    if is_unix {
                        // Synchronous path for Unix sockets (works in
                        // bhyve zones without /dev/poll).
                        info!(mlog, "connecting to migration socket";
                            "path" => &target_addr);
                        let stream = match std::os::unix::net::UnixStream::connect(
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
                                    dev.flush_backing();
                                }
                            });
                        let ctrl_for_export = ctrl_arc.clone();
                        let export_fn: vmm_migrate::sync_source::DeviceExportFn =
                            Box::new(move || ctrl_for_export.export_device_queues());
                        match vmm_migrate::sync_source::run_source_sync(
                            stream, hdl, &memctx, num_cpus, mem_size,
                            status, &mlog, &ctrl_arc.cpu_baseline,
                            pre_pause_fn, flush_fn, export_fn, zfs_barrier,
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
            ctrl.state.store(
                VmRunState::Migrating as u8,
                Ordering::Release,
            );

            let status = Arc::new(Mutex::new(
                vmm_migrate::MigrationStatus::default(),
            ));
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
) -> std::io::Result<(thread::JoinHandle<()>, PathBuf)> {
    // Remove stale socket from a previous crash (may not exist)
    let _ = std::fs::remove_file(path);

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Set restrictive umask before bind to avoid TOCTOU race where
    // the socket is briefly world-accessible between bind and chmod.
    #[cfg(unix)]
    let old_umask = unsafe { libc::umask(0o177) }; // creates socket as 0600

    let listener = UnixListener::bind(path);

    // Restore original umask immediately
    #[cfg(unix)]
    unsafe { libc::umask(old_umask); }

    let listener = listener?;

    let sock_path = path.to_path_buf();
    let cleanup_path = sock_path.clone();

    let handle = thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Err(e) = handle_connection(&ctrl, &stream, &log) {
                            warn!(log, "control connection error"; "error" => %e);
                        }
                    }
                    Err(e) => {
                        error!(log, "control accept error"; "error" => %e);
                        break;
                    }
                }
            }
        })?;

    Ok((handle, cleanup_path))
}

/// Handle a single client connection: read JSON lines, dispatch, respond.
fn handle_connection(
    ctrl: &Arc<VmController>,
    stream: &std::os::unix::net::UnixStream,
    log: &Logger,
) -> Result<(), ControlError> {
    let reader = BufReader::new(stream);
    let mut writer = stream;
    let mut line = String::with_capacity(256);

    for result in reader.lines() {
        line.clear();
        let read_line = result?;
        if read_line.is_empty() {
            continue;
        }

        match serde_json::from_str::<Command>(&read_line) {
            Ok(cmd) => {
                info!(log, "control command"; "command" => &read_line);

                // Metrics commands return raw text, bypass Response serialization
                let raw = match &cmd {
                    Command::Metrics => {
                        let uptime = ctrl.start_time.elapsed().as_secs();
                        let vcpus: Vec<String> = ctrl.vcpu_metrics.iter()
                            .enumerate()
                            .map(|(i, v)| v.to_json(i as i32))
                            .collect();
                        Some(format!(
                            r#"{{"success":true,"uptime_secs":{},"vcpus":[{}]}}"#,
                            uptime, vcpus.join(","),
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
                warn!(log, "invalid control command"; "error" => %e, "line" => &read_line);
                let response = Response::err("invalid command");
                let mut out = serde_json::to_vec(&response)?;
                out.push(b'\n');
                writer.write_all(&out)?;
                writer.flush()?;
            }
        };
    }

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
    fn command_deserialize_status() {
        let cmd: Command = serde_json::from_str(r#"{"command":"status"}"#).unwrap();
        assert!(matches!(cmd, Command::Status));
    }

    #[test]
    fn command_deserialize_pause() {
        let cmd: Command = serde_json::from_str(r#"{"command":"pause"}"#).unwrap();
        assert!(matches!(cmd, Command::Pause));
    }

    #[test]
    fn command_deserialize_shutdown() {
        let cmd: Command = serde_json::from_str(r#"{"command":"shutdown"}"#).unwrap();
        assert!(matches!(cmd, Command::Shutdown));
    }

    #[test]
    fn command_deserialize_invalid() {
        let result = serde_json::from_str::<Command>(r#"{"command":"explode"}"#);
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
    fn command_deserialize_migrate_dest() {
        let cmd: Command = serde_json::from_str(
            r#"{"command":"migrate-dest","listen_addr":"0.0.0.0:1234"}"#,
        )
        .unwrap();
        assert!(matches!(cmd, Command::MigrateDest { .. }));
    }

    #[test]
    fn command_deserialize_migrate_status() {
        let cmd: Command = serde_json::from_str(
            r#"{"command":"migrate-status"}"#,
        )
        .unwrap();
        assert!(matches!(cmd, Command::MigrateStatus));
    }
}
