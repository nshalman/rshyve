// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! vCPU thread management.
//!
//! Each vCPU runs on a dedicated OS thread, executing a run loop that:
//! 1. Enters the guest via `vcpu.enter()`
//! 2. Parses the VM exit
//! 3. Dispatches to the PIO or MMIO bus
//! 4. Constructs the re-entry command
//! 5. Repeats
//!
//! The loop terminates on suspend events (halt, reset, triple-fault)
//! which are sent to the main thread via a channel.

use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use slog::{debug, error, info, warn, Logger};

use vmm_core::exits::{
    InoutReq, InoutRes, MmioReq, MmioRes, Suspend,
    VmEntry, VmExitKind,
};
use vmm_core::inst_emul;
use vmm_core::mem::PhysMap;
use vmm_core::pio::PioBus;
use vmm_core::mmio::MmioBus;
use vmm_core::vcpu::Vcpu;

/// Events sent from vCPU threads to the main thread.
#[derive(Debug)]
pub enum VcpuEvent {
    /// vCPU suspended (halt, reset, or triple-fault).
    Suspended { vcpu_id: i32, kind: Suspend },
    /// vCPU encountered a fatal error.
    Error { vcpu_id: i32, error: String },
}

/// Spawn a thread for each vCPU and run the exit-dispatch loop.
///
/// Returns a channel receiver for vCPU events and the join handles.
pub fn spawn_vcpu_threads(
    vcpus: &[Vcpu],
    bus_pio: &Arc<PioBus>,
    bus_mmio: &Arc<MmioBus>,
    physmap: &Arc<PhysMap>,
    api_version: u32,
    log: &Logger,
) -> (mpsc::Receiver<VcpuEvent>, Vec<thread::JoinHandle<()>>) {
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::with_capacity(vcpus.len());

    for vcpu in vcpus {
        let vcpu_id = vcpu.id();
        let vcpu_log = log.new(slog::o!("vcpu" => vcpu_id));

        // Clone Arcs for the thread
        let bus_pio = Arc::clone(bus_pio);
        let bus_mmio = Arc::clone(bus_mmio);
        let physmap = Arc::clone(physmap);
        let tx = tx.clone();
        let hdl = Arc::clone(vcpu.hdl());

        // We need to create a new Vcpu for the thread since Vcpu
        // is not Send (it holds an Arc but we need the ioctl fd).
        // Actually, Vcpu IS Send because it only holds Arc<VmmHdl>.
        // But we can't move the borrow — create a new handle.
        let thread_vcpu = vmm_core::Vcpu::new_for_thread(vcpu_id, hdl);

        let handle = thread::Builder::new()
            .name(format!("vcpu-{}", vcpu_id))
            .spawn(move || {
                vcpu_run_loop(
                    &thread_vcpu,
                    &bus_pio,
                    &bus_mmio,
                    &physmap,
                    api_version,
                    &tx,
                    &vcpu_log,
                );
            })
            .expect("failed to spawn vCPU thread");

        handles.push(handle);
    }

    (rx, handles)
}

/// The main vCPU execution loop.
///
/// Runs until a suspend event occurs or a fatal error is encountered.
fn vcpu_run_loop(
    vcpu: &Vcpu,
    bus_pio: &PioBus,
    bus_mmio: &MmioBus,
    physmap: &PhysMap,
    api_version: u32,
    events: &mpsc::Sender<VcpuEvent>,
    log: &Logger,
) {
    let vcpu_id = vcpu.id();
    debug!(log, "vCPU run loop starting");

    let mut next_entry = VmEntry::Run;
    info!(log, "vCPU run loop starting");
    let mut exit_count: u64 = 0;
    let mut pio_count: u64 = 0;
    let mut mmio_count: u64 = 0;
    let mut bogus_count: u64 = 0;
    let mut inst_emul_count: u64 = 0;
    let start_time = std::time::Instant::now();

    loop {
        let exit = match vcpu.enter(&next_entry, api_version) {
            Ok(exit) => exit,
            Err(e) => {
                // EINTR is normal (e.g., signal delivery) — retry
                if e.raw_os_error() == Some(libc::EINTR) {
                    next_entry = VmEntry::Run;
                    continue;
                }
                error!(log, "VM_RUN failed"; "error" => %e);
                let _ = events.send(VcpuEvent::Error {
                    vcpu_id,
                    error: e.to_string(),
                });
                return;
            }
        };

        exit_count += 1;

        // Periodic stats
        if exit_count % 10_000 == 0 {
            let elapsed = start_time.elapsed();
            eprintln!(
                "[stats] exits={} pio={} mmio={} bogus={} inst_emul={} ms={} rip={:#x}",
                exit_count, pio_count, mmio_count, bogus_count,
                inst_emul_count, elapsed.as_millis(), exit.rip,
            );
        }

        next_entry = match exit.kind {
            VmExitKind::Bogus | VmExitKind::Debug => {
                bogus_count += 1;
                VmEntry::Run
            }

            VmExitKind::Inout(ref req) => {
                pio_count += 1;
                // Trace kernel-phase PIO accesses
                if exit_count > 30000 && exit_count < 30020 {
                    eprintln!("[pio-trace] #{} {:?} rip={:#x}", exit_count, req, exit.rip);
                }
                let res = handle_inout(req, bus_pio);
                VmEntry::InoutFulfill(res)
            }

            VmExitKind::Mmio(req) => {
                mmio_count += 1;
                let res = handle_mmio(&req, bus_mmio);
                VmEntry::MmioFulfill(res)
            }

            VmExitKind::Rdmsr(code) => {
                // Unhandled MSRs: return 0 in RAX:RDX.
                // The kernel auto-advances RIP past the RDMSR instruction.
                let _ = vcpu.set_reg(
                    bhyve_api::vm_reg_name::VM_REG_GUEST_RAX, 0,
                );
                let _ = vcpu.set_reg(
                    bhyve_api::vm_reg_name::VM_REG_GUEST_RDX, 0,
                );
                VmEntry::Run
            }

            VmExitKind::Wrmsr(_code, _val) => {
                // Unhandled MSRs: silently ignore the write.
                // The kernel auto-advances RIP past the WRMSR instruction.
                VmEntry::Run
            }

            VmExitKind::Suspended(detail) => {
                info!(log, "vCPU suspended";
                    "kind" => ?detail.kind,
                    "when_ns" => detail.when.as_nanos(),
                );
                let _ = events.send(VcpuEvent::Suspended {
                    vcpu_id,
                    kind: detail.kind,
                });
                return;
            }

            VmExitKind::InstEmul { inst, num_valid } => {
                inst_emul_count += 1;
                match inst_emul::emulate_inst(
                    vcpu, exit.rip, &inst, num_valid,
                    physmap, bus_mmio,
                ) {
                    Ok(true) => {
                        // Successfully emulated — re-enter without
                        // advancing RIP (emulator already did it)
                        VmEntry::Run
                    }
                    Ok(false) => {
                        // Instruction not supported
                        let inst_hex: String = inst[..num_valid as usize]
                            .iter()
                            .map(|b| format!("{:02x}", b))
                            .collect::<Vec<_>>()
                            .join(" ");
                        error!(log, "unsupported instruction emulation";
                            "rip" => format!("{:#x}", exit.rip),
                            "bytes" => &inst_hex,
                        );
                        let _ = events.send(VcpuEvent::Error {
                            vcpu_id,
                            error: format!(
                                "unsupported instruction at RIP {:#x}: [{}]",
                                exit.rip, inst_hex,
                            ),
                        });
                        return;
                    }
                    Err(e) => {
                        error!(log, "instruction emulation failed";
                            "rip" => format!("{:#x}", exit.rip),
                            "error" => %e,
                        );
                        let _ = events.send(VcpuEvent::Error {
                            vcpu_id,
                            error: format!(
                                "instruction emulation failed at {:#x}: {}",
                                exit.rip, e,
                            ),
                        });
                        return;
                    }
                }
            }

            VmExitKind::Hlt => {
                // HLT: vCPU is idle. In a full VMM this would be handled
                // by the kernel (hlt-exit is disabled by default in API v6+).
                // If we get here, just re-enter.
                VmEntry::Run
            }

            VmExitKind::VmxError(status) => {
                error!(log, "VMX error"; "status" => status);
                let _ = events.send(VcpuEvent::Error {
                    vcpu_id,
                    error: format!("VMX error, status {}", status),
                });
                return;
            }

            VmExitKind::SvmError(code) => {
                error!(log, "SVM error"; "code" => code);
                let _ = events.send(VcpuEvent::Error {
                    vcpu_id,
                    error: format!("SVM error, code {}", code),
                });
                return;
            }

            VmExitKind::Paging(gpa, fault_type) => {
                warn!(log, "unhandled paging exit";
                    "gpa" => format!("{:#x}", gpa),
                    "fault_type" => fault_type,
                );
                // Treat as fatal
                let _ = events.send(VcpuEvent::Error {
                    vcpu_id,
                    error: format!(
                        "unhandled paging fault at GPA {:#x}",
                        gpa
                    ),
                });
                return;
            }

            VmExitKind::Unknown(code) => {
                error!(log, "unknown VM exit"; "code" => code);
                let _ = events.send(VcpuEvent::Error {
                    vcpu_id,
                    error: format!("unknown exit code {}", code),
                });
                return;
            }
        };
    }
}

/// Dispatch an IN/OUT instruction to the PIO bus.
fn handle_inout(req: &InoutReq, bus: &PioBus) -> InoutRes {
    match req {
        InoutReq::In(io) => {
            let val = bus.handle_in(io.port, io.bytes);
            InoutRes::In(*io, val)
        }
        InoutReq::Out(io, val) => {
            bus.handle_out(io.port, io.bytes, *val);
            InoutRes::Out(*io)
        }
    }
}

/// Dispatch an MMIO access to the MMIO bus.
fn handle_mmio(req: &MmioReq, bus: &MmioBus) -> MmioRes {
    match req {
        MmioReq::Read(r) => {
            let data = bus.handle_read(r.addr, r.bytes);
            MmioRes::Read {
                addr: r.addr,
                data,
                bytes: r.bytes,
            }
        }
        MmioReq::Write(w) => {
            bus.handle_write(w.addr, w.bytes, w.data);
            MmioRes::Write {
                addr: w.addr,
                bytes: w.bytes,
            }
        }
    }
}
