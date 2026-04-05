// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VM exit and entry types.
//!
//! When a vCPU exits to userspace, the kernel populates a `vm_exit`
//! struct with the reason and associated data. This module provides
//! safe Rust types (`VmExit`, `VmExitKind`) that parse the raw kernel
//! struct, and `VmEntry` which constructs the re-entry command.
//!
//! # Safety
//!
//! The raw `vm_exit` struct contains a union (`vm_exit_payload`). We
//! read from the union variant corresponding to the exit code. This
//! is safe because the kernel guarantees the exit code matches the
//! active variant.

use std::os::raw::c_void;
use std::time::Duration;

use bhyve_api::{
    vm_entry, vm_entry_cmds, vm_entry_payload, vm_exit, vm_exitcode,
    vm_suspend_how, ApiVersion, INOUT_IN,
};

/// Parsed VM exit information.
#[derive(Debug)]
pub struct VmExit {
    /// Guest instruction pointer at the time of exit.
    pub rip: u64,
    /// Length of the instruction that triggered the exit (0 if unknown).
    pub inst_len: u8,
    /// The kind of exit.
    pub kind: VmExitKind,
}

impl VmExit {
    /// Parse a raw `vm_exit` struct into a safe `VmExit`.
    pub fn parse(exit: &vm_exit, api_version: u32) -> Self {
        Self {
            rip: exit.rip,
            inst_len: exit.inst_length as u8,
            kind: VmExitKind::parse(exit, api_version),
        }
    }
}

/// I/O port identifier.
#[derive(Copy, Clone, Debug)]
pub struct IoPort {
    pub port: u16,
    pub bytes: u8,
}

/// An IN or OUT instruction request.
#[derive(Copy, Clone, Debug)]
pub enum InoutReq {
    /// Guest executed IN (read from port).
    In(IoPort),
    /// Guest executed OUT (write to port with value).
    Out(IoPort, u32),
}

/// MMIO read request.
#[derive(Copy, Clone, Debug)]
pub struct MmioReadReq {
    pub addr: u64,
    pub bytes: u8,
}

/// MMIO write request.
#[derive(Copy, Clone, Debug)]
pub struct MmioWriteReq {
    pub addr: u64,
    pub data: u64,
    pub bytes: u8,
}

/// An MMIO read or write request.
#[derive(Copy, Clone, Debug)]
pub enum MmioReq {
    Read(MmioReadReq),
    Write(MmioWriteReq),
}

/// VM suspend reason.
#[derive(Copy, Clone, Debug)]
pub enum Suspend {
    Halt,
    Reset,
    TripleFault(i32),
}

/// VM suspend detail with timing.
#[derive(Copy, Clone, Debug)]
pub struct SuspendDetail {
    pub kind: Suspend,
    pub when: Duration,
}

/// The specific kind of VM exit.
#[derive(Debug)]
pub enum VmExitKind {
    /// Spurious exit (no action needed, re-enter immediately).
    Bogus,
    /// Port I/O instruction (IN/OUT).
    Inout(InoutReq),
    /// Memory-mapped I/O access.
    Mmio(MmioReq),
    /// RDMSR instruction (read model-specific register).
    Rdmsr(u32),
    /// WRMSR instruction (write model-specific register).
    Wrmsr(u32, u64),
    /// VM suspended (halt, reset, triple-fault).
    Suspended(SuspendDetail),
    /// VMX-specific error (Intel).
    VmxError(i32),
    /// SVM-specific error (AMD).
    SvmError(u64),
    /// Debug breakpoint.
    Debug,
    /// Guest page fault.
    Paging(u64, i32),
    /// Instruction emulation required (MMIO via decoded instruction).
    InstEmul {
        /// The instruction bytes (up to 15).
        inst: [u8; 15],
        /// Number of valid instruction bytes.
        num_valid: u8,
    },
    /// HLT instruction.
    Hlt,
    /// Unrecognized exit code.
    Unknown(i32),
}

impl VmExitKind {
    /// Numeric exit code for DTrace probes.
    pub fn exit_code(&self) -> u32 {
        match self {
            Self::Bogus => 0,
            Self::Inout(_) => 1,
            Self::Mmio(_) => 2,
            Self::Rdmsr(_) => 3,
            Self::Wrmsr(_, _) => 4,
            Self::Suspended(_) => 5,
            Self::VmxError(_) => 6,
            Self::SvmError(_) => 7,
            Self::Debug => 8,
            Self::Paging(_, _) => 9,
            Self::InstEmul { .. } => 10,
            Self::Hlt => 11,
            Self::Unknown(c) => *c as u32,
        }
    }

    /// Parse the raw `vm_exit` into a `VmExitKind`.
    ///
    /// # Safety (internal)
    ///
    /// Reads from the `vm_exit.u` union based on the exit code. The
    /// bhyve kernel guarantees the union variant matches the exit code.
    pub fn parse(exit: &vm_exit, api_version: u32) -> Self {
        let code = match vm_exitcode::from_repr(exit.exitcode) {
            None => return VmExitKind::Unknown(exit.exitcode),
            Some(c) => c,
        };

        match code {
            vm_exitcode::VM_EXITCODE_BOGUS => VmExitKind::Bogus,

            vm_exitcode::VM_EXITCODE_DEPRECATED2 => {
                // Pre-v16: REQIDLE, treat as bogus. Post-v16: unexpected.
                if api_version < ApiVersion::V16 as u32 {
                    VmExitKind::Bogus
                } else {
                    VmExitKind::Unknown(code as i32)
                }
            }

            vm_exitcode::VM_EXITCODE_INOUT => {
                // Safety: exit code guarantees the inout variant is active.
                let inout = unsafe { &exit.u.inout };
                let port = IoPort { port: inout.port, bytes: inout.bytes };
                if inout.flags & INOUT_IN != 0 {
                    VmExitKind::Inout(InoutReq::In(port))
                } else {
                    VmExitKind::Inout(InoutReq::Out(port, inout.eax))
                }
            }

            vm_exitcode::VM_EXITCODE_RDMSR => {
                let msr = unsafe { &exit.u.msr };
                VmExitKind::Rdmsr(msr.code)
            }

            vm_exitcode::VM_EXITCODE_WRMSR => {
                let msr = unsafe { &exit.u.msr };
                VmExitKind::Wrmsr(msr.code, msr.wval)
            }

            vm_exitcode::VM_EXITCODE_MMIO => {
                let mmio = unsafe { &exit.u.mmio };
                if mmio.read != 0 {
                    VmExitKind::Mmio(MmioReq::Read(MmioReadReq {
                        addr: mmio.gpa,
                        bytes: mmio.bytes,
                    }))
                } else {
                    VmExitKind::Mmio(MmioReq::Write(MmioWriteReq {
                        addr: mmio.gpa,
                        data: mmio.data,
                        bytes: mmio.bytes,
                    }))
                }
            }

            vm_exitcode::VM_EXITCODE_SUSPENDED => {
                let detail = unsafe { &exit.u.suspend };
                let valid_detail =
                    api_version >= ApiVersion::V16 as u32;
                let kind =
                    match vm_suspend_how::from_repr(detail.how as u32) {
                        Some(vm_suspend_how::VM_SUSPEND_RESET) => {
                            Suspend::Reset
                        }
                        Some(vm_suspend_how::VM_SUSPEND_POWEROFF)
                        | Some(vm_suspend_how::VM_SUSPEND_HALT) => {
                            Suspend::Halt
                        }
                        Some(vm_suspend_how::VM_SUSPEND_TRIPLEFAULT) => {
                            let src = if valid_detail {
                                detail.source
                            } else {
                                -1
                            };
                            Suspend::TripleFault(src)
                        }
                        _ => {
                            return VmExitKind::Unknown(exit.exitcode);
                        }
                    };
                let when = Duration::from_nanos(
                    if valid_detail { detail.when } else { 0 },
                );
                VmExitKind::Suspended(SuspendDetail { kind, when })
            }

            vm_exitcode::VM_EXITCODE_VMX => {
                let vmx = unsafe { &exit.u.vmx };
                VmExitKind::VmxError(vmx.status)
            }

            vm_exitcode::VM_EXITCODE_SVM => {
                let svm = unsafe { &exit.u.svm };
                VmExitKind::SvmError(svm.exitcode)
            }

            vm_exitcode::VM_EXITCODE_PAGING => {
                let paging = unsafe { &exit.u.paging };
                VmExitKind::Paging(paging.gpa, paging.fault_type)
            }

            vm_exitcode::VM_EXITCODE_INST_EMUL => {
                let inst = unsafe { &exit.u.inst_emul };
                VmExitKind::InstEmul {
                    inst: inst.inst,
                    num_valid: inst.num_valid,
                }
            }

            vm_exitcode::VM_EXITCODE_HLT => VmExitKind::Hlt,

            vm_exitcode::VM_EXITCODE_DEBUG => VmExitKind::Debug,

            // Internal kernel exits or unhandled — return Unknown
            // rather than panicking for robustness.
            _ => VmExitKind::Unknown(code as i32),
        }
    }
}

// ---------------------------------------------------------------
// VM entry types (re-entering the guest after handling an exit)
// ---------------------------------------------------------------

/// Result of handling an IN/OUT exit.
#[derive(Copy, Clone, Debug)]
pub enum InoutRes {
    /// Completed IN: port + data read from device.
    In(IoPort, u32),
    /// Completed OUT: port (write was consumed).
    Out(IoPort),
}

impl InoutRes {
    /// Create a result for a failed (unhandled) I/O operation.
    /// Reads return all-ones; writes are silently consumed.
    pub fn emulate_failed(req: &InoutReq) -> Self {
        match req {
            InoutReq::In(port) => InoutRes::In(*port, !0u32),
            InoutReq::Out(port, _) => InoutRes::Out(*port),
        }
    }
}

/// Result of handling an MMIO exit.
#[derive(Copy, Clone, Debug)]
pub enum MmioRes {
    Read { addr: u64, data: u64, bytes: u8 },
    Write { addr: u64, bytes: u8 },
}

impl MmioRes {
    /// Create a result for a failed (unhandled) MMIO operation.
    pub fn emulate_failed(req: &MmioReq) -> Self {
        match req {
            MmioReq::Read(r) => MmioRes::Read {
                addr: r.addr,
                data: !0u64,
                bytes: r.bytes,
            },
            MmioReq::Write(w) => MmioRes::Write {
                addr: w.addr,
                bytes: w.bytes,
            },
        }
    }
}

/// Command for re-entering the guest vCPU.
#[derive(Debug)]
pub enum VmEntry {
    /// Normal re-entry (no pending fulfillment).
    Run,
    /// Re-enter with IN/OUT result.
    InoutFulfill(InoutRes),
    /// Re-enter with MMIO result.
    MmioFulfill(MmioRes),
}

impl VmEntry {
    /// Convert to the raw `vm_entry` struct for the kernel ioctl.
    ///
    /// # Arguments
    /// - `cpuid`: vCPU identifier
    /// - `exit_ptr`: pointer to the `vm_exit` struct for the kernel
    ///   to write exit data into
    pub fn to_raw(
        &self,
        cpuid: i32,
        exit_ptr: *mut vm_exit,
    ) -> vm_entry {
        let mut payload = vm_entry_payload::default();
        let cmd = match self {
            VmEntry::Run => vm_entry_cmds::VEC_DEFAULT,

            VmEntry::InoutFulfill(res) => {
                let io = match res {
                    InoutRes::In(io, val) => {
                        payload.inout.flags = INOUT_IN;
                        payload.inout.eax = *val;
                        io
                    }
                    InoutRes::Out(io) => {
                        payload.inout.flags = 0;
                        payload.inout.eax = 0;
                        io
                    }
                };
                payload.inout.port = io.port;
                payload.inout.bytes = io.bytes;
                vm_entry_cmds::VEC_FULFILL_INOUT
            }

            VmEntry::MmioFulfill(res) => {
                let (addr, bytes) = match res {
                    MmioRes::Read { addr, data, bytes } => {
                        payload.mmio.read = 1;
                        payload.mmio.data = *data;
                        (*addr, *bytes)
                    }
                    MmioRes::Write { addr, bytes, .. } => {
                        (*addr, *bytes)
                    }
                };
                payload.mmio.gpa = addr;
                payload.mmio.bytes = bytes;
                vm_entry_cmds::VEC_FULFILL_MMIO
            }
        };

        vm_entry {
            cpuid,
            cmd: cmd as u32,
            u: payload,
            exit_data: exit_ptr as *mut c_void,
        }
    }
}
