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
            _ => Self::Stopped,
        }
    }
}

/// Shared VM state, readable from any thread without locking.
pub struct VmController {
    state: AtomicU8,
    hdl: Arc<VmmHdl>,
    physmap: Arc<PhysMap>,
    vm_name: &'static str,
    num_cpus: u32,
    mem_size: usize,
    start_time: Instant,
    migrate_status: Mutex<Option<Arc<Mutex<vmm_migrate::MigrationStatus>>>>,
    /// Per-vCPU metrics, shared with vCPU threads.
    vcpu_metrics: Vec<Arc<vmm_devices::metrics::VcpuMetrics>>,
}

impl VmController {
    pub fn new(
        hdl: Arc<VmmHdl>,
        physmap: Arc<PhysMap>,
        vm_name: String,
        num_cpus: u32,
        mem_size: usize,
        vcpu_metrics: Vec<Arc<vmm_devices::metrics::VcpuMetrics>>,
    ) -> Arc<Self> {
        let name: &'static str = Box::leak(vm_name.into_boxed_str());
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
    MigrateSource { target_addr: String },
    #[serde(rename = "migrate-dest")]
    MigrateDest { listen_addr: String },
    #[serde(rename = "migrate-status")]
    MigrateStatus,
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
    vm_name: Option<&'static str>,
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
        }
    }
}

// ── Command dispatch ────────────────────────────────────────────────

fn dispatch(ctrl: &VmController, cmd: Command) -> Response {
    match cmd {
        Command::Status => {
            let mut r = Response::ok();
            r.state = Some(ctrl.state());
            // vm_name is owned by ctrl; we leak a &'static str for serde.
            // This is fine — the controller lives for the process lifetime.
            r.vm_name = Some(ctrl.vm_name);
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
                    let mut r = Response::ok();
                    r.state = Some(VmRunState::Paused);
                    r
                }
                Err(e) => {
                    // Rollback state
                    ctrl.state.store(VmRunState::Running as u8, Ordering::Release);
                    eprintln!("[control] pause failed: {e}");
                    Response::err("pause ioctl failed")
                }
            }
        }

        Command::Resume => {
            if !ctrl.transition(VmRunState::Paused, VmRunState::Running) {
                return Response::err("VM is not paused");
            }
            match ctrl.hdl.resume() {
                Ok(()) => {
                    let mut r = Response::ok();
                    r.state = Some(VmRunState::Running);
                    r
                }
                Err(e) => {
                    ctrl.state.store(VmRunState::Paused as u8, Ordering::Release);
                    eprintln!("[control] resume failed: {e}");
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
                    eprintln!("[control] shutdown failed: {e}");
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
                    eprintln!("[control] stop failed: {e}");
                    Response::err("suspend ioctl failed")
                }
            }
        }

        Command::MigrateSource { target_addr } => {
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

            let hdl = Arc::clone(&ctrl.hdl);
            let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
            let num_cpus = ctrl.num_cpus;
            let mem_size = ctrl.mem_size as u64;
            // We need a reference to the AtomicU8 for the spawned thread
            // to restore state on failure. Use a raw pointer (safe because
            // VmController lives for the process lifetime).
            let state_ptr = &ctrl.state as *const AtomicU8 as usize;

            if let Err(e) = thread::Builder::new()
                .name("migrate-source".into())
                .spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            eprintln!("[migrate] failed to create runtime: {e}");
                            // Safety: VmController outlives this thread.
                            let state = unsafe {
                                &*(state_ptr as *const AtomicU8)
                            };
                            state.store(
                                VmRunState::Running as u8,
                                Ordering::Release,
                            );
                            return;
                        }
                    };

                    rt.block_on(async {
                        let stream = match tokio::net::TcpStream::connect(
                            &target_addr,
                        )
                        .await
                        {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!(
                                    "[migrate] connect to {target_addr} \
                                     failed: {e}"
                                );
                                let state = unsafe {
                                    &*(state_ptr as *const AtomicU8)
                                };
                                state.store(
                                    VmRunState::Running as u8,
                                    Ordering::Release,
                                );
                                return;
                            }
                        };

                        match vmm_migrate::source::run_source(
                            stream,
                            hdl,
                            &memctx,
                            num_cpus,
                            mem_size,
                            status,
                        )
                        .await
                        {
                            Ok(()) => {
                                eprintln!(
                                    "[migrate] source migration complete"
                                );
                                // VM is paused and should be destroyed.
                                // Transition to Stopped.
                                let state = unsafe {
                                    &*(state_ptr as *const AtomicU8)
                                };
                                state.store(
                                    VmRunState::Stopped as u8,
                                    Ordering::Release,
                                );
                            }
                            Err(e) => {
                                eprintln!(
                                    "[migrate] source migration failed: {e}"
                                );
                                // run_source already resumed on failure.
                                let state = unsafe {
                                    &*(state_ptr as *const AtomicU8)
                                };
                                state.store(
                                    VmRunState::Running as u8,
                                    Ordering::Release,
                                );
                            }
                        }
                    });
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

            let hdl = Arc::clone(&ctrl.hdl);
            let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
            let num_cpus = ctrl.num_cpus;
            let mem_size = ctrl.mem_size as u64;
            let state_ptr = &ctrl.state as *const AtomicU8 as usize;

            if let Err(e) = thread::Builder::new()
                .name("migrate-dest".into())
                .spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            eprintln!("[migrate] failed to create runtime: {e}");
                            let state = unsafe {
                                &*(state_ptr as *const AtomicU8)
                            };
                            state.store(
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
                                eprintln!(
                                    "[migrate] bind {listen_addr} failed: {e}"
                                );
                                let state = unsafe {
                                    &*(state_ptr as *const AtomicU8)
                                };
                                state.store(
                                    VmRunState::Stopped as u8,
                                    Ordering::Release,
                                );
                                return;
                            }
                        };

                        let (stream, peer) = match listener.accept().await {
                            Ok(v) => v,
                            Err(e) => {
                                eprintln!(
                                    "[migrate] accept failed: {e}"
                                );
                                let state = unsafe {
                                    &*(state_ptr as *const AtomicU8)
                                };
                                state.store(
                                    VmRunState::Stopped as u8,
                                    Ordering::Release,
                                );
                                return;
                            }
                        };

                        eprintln!(
                            "[migrate] accepted connection from {peer}"
                        );

                        match vmm_migrate::destination::run_destination(
                            stream,
                            hdl,
                            &memctx,
                            num_cpus,
                            mem_size,
                            status,
                        )
                        .await
                        {
                            Ok(()) => {
                                eprintln!(
                                    "[migrate] destination migration \
                                     complete, VM running"
                                );
                                let state = unsafe {
                                    &*(state_ptr as *const AtomicU8)
                                };
                                state.store(
                                    VmRunState::Running as u8,
                                    Ordering::Release,
                                );
                            }
                            Err(e) => {
                                eprintln!(
                                    "[migrate] destination migration \
                                     failed: {e}"
                                );
                                let state = unsafe {
                                    &*(state_ptr as *const AtomicU8)
                                };
                                state.store(
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
    }
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
    // Remove stale socket from a previous crash
    let _ = std::fs::remove_file(path);

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(path)?;
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
    ctrl: &VmController,
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
