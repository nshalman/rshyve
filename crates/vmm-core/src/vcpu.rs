// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-vCPU execution context.
//!
//! Each vCPU runs in its own OS thread, calling [`Vcpu::run_loop`] or
//! the lower-level [`Vcpu::enter`] in a loop. The kernel returns
//! control on VM exits (I/O, MMIO, MSR, halt, etc.) which the caller
//! dispatches to the appropriate device handler.
//!
//! # Thread safety
//!
//! The bhyve kernel enforces that only one thread may call VM_RUN for
//! a given vCPU at a time. This type does not enforce that invariant
//! in Rust — the caller must ensure single-threaded access per vCPU.

use std::os::fd::AsRawFd;
use std::sync::Arc;

use bhyve_api::{
    ioctls,
    vm_exit, vm_register, vm_run_state, vm_seg_desc, seg_desc,
    vm_reg_name, VRS_RUN,
};

use crate::exits::{VmEntry, VmExit};
use crate::hdl::VmmHdl;

/// Represents a single virtual CPU.
///
/// Provides methods to activate, configure registers, and run the vCPU.
/// The vCPU must be activated before it can be run.
pub struct Vcpu {
    id: i32,
    hdl: Arc<VmmHdl>,
}

impl Vcpu {
    /// Create a new vCPU handle.
    pub(crate) fn new(id: i32, hdl: Arc<VmmHdl>) -> Self {
        Self { id, hdl }
    }

    /// Create a vCPU handle for use in a dedicated thread.
    ///
    /// This creates a new handle sharing the same VmmHdl, suitable for
    /// moving to a thread that will call [`enter`] in a loop.
    pub fn new_for_thread(id: i32, hdl: Arc<VmmHdl>) -> Self {
        Self { id, hdl }
    }

    /// Get the vCPU identifier.
    pub fn id(&self) -> i32 {
        self.id
    }

    /// Get a reference to the VM handle.
    pub fn hdl(&self) -> &Arc<VmmHdl> {
        &self.hdl
    }

    /// Activate this vCPU in the kernel.
    ///
    /// Must be called before [`enter`]. The kernel allocates per-vCPU
    /// state on activation.
    pub fn activate(&self) -> std::io::Result<()> {
        let fd = self.hdl.as_raw_fd();
        let mut cpu = self.id;
        // Safety: VM_ACTIVATE_CPU takes a pointer to the vCPU id.
        let res = unsafe {
            libc::ioctl(fd, ioctls::VM_ACTIVATE_CPU as _, &mut cpu)
        };
        if res == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    // ---------------------------------------------------------------
    // Register access
    // ---------------------------------------------------------------

    /// Set a guest register value.
    pub fn set_reg(
        &self,
        reg: vm_reg_name,
        val: u64,
    ) -> std::io::Result<()> {
        let mut vr = vm_register {
            cpuid: self.id,
            regnum: reg as i32,
            regval: val,
        };
        let fd = self.hdl.as_raw_fd();
        // Safety: vm_register is the correct struct for VM_SET_REGISTER.
        let res = unsafe {
            libc::ioctl(fd, ioctls::VM_SET_REGISTER as _, &mut vr)
        };
        if res == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Get a guest register value.
    pub fn get_reg(
        &self,
        reg: vm_reg_name,
    ) -> std::io::Result<u64> {
        let mut vr = vm_register {
            cpuid: self.id,
            regnum: reg as i32,
            regval: 0,
        };
        let fd = self.hdl.as_raw_fd();
        // Safety: vm_register is the correct struct for VM_GET_REGISTER.
        let res = unsafe {
            libc::ioctl(fd, ioctls::VM_GET_REGISTER as _, &mut vr)
        };
        if res == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(vr.regval)
    }

    /// Set the vCPU run state (e.g., VRS_RUN to mark it runnable).
    pub fn set_run_state(
        &self,
        state: u32,
    ) -> std::io::Result<()> {
        let mut rs = vm_run_state {
            vcpuid: self.id,
            state,
            sipi_vector: 0,
            _pad: [0; 3],
        };
        let fd = self.hdl.as_raw_fd();
        // Safety: vm_run_state is the correct struct for VM_SET_RUN_STATE.
        let res = unsafe {
            libc::ioctl(fd, ioctls::VM_SET_RUN_STATE as _, &mut rs)
        };
        if res == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Reset all vCPU state to x86 power-on defaults.
    ///
    /// This issues `VM_RESET_CPU` which initializes all registers,
    /// segment descriptors, and pending interrupts to their reset
    /// values as defined by the x86 architecture.
    pub fn reboot_state(&self) -> std::io::Result<()> {
        let mut vvr = bhyve_api::vm_vcpu_reset {
            vcpuid: self.id,
            kind: bhyve_api::vcpu_reset_kind::VRK_RESET as u32,
        };
        let fd = self.hdl.as_raw_fd();
        // Safety: vm_vcpu_reset is the correct struct for VM_RESET_CPU.
        let res = unsafe {
            libc::ioctl(fd, ioctls::VM_RESET_CPU as _, &mut vvr)
        };
        if res == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Set a guest segment descriptor (CS, DS, SS, ES, FS, GS, TR, LDTR, GDTR, IDTR).
    pub fn set_segment_desc(
        &self,
        reg: vm_reg_name,
        desc: &seg_desc,
    ) -> std::io::Result<()> {
        let mut vsd = vm_seg_desc {
            cpuid: self.id,
            regnum: reg as i32,
            desc: *desc,
        };
        let fd = self.hdl.as_raw_fd();
        // Safety: vm_seg_desc is the correct struct for VM_SET_SEGMENT_DESCRIPTOR.
        let res = unsafe {
            libc::ioctl(fd, ioctls::VM_SET_SEGMENT_DESCRIPTOR as _, &mut vsd)
        };
        if res == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Set up the BSP (bootstrap processor, vCPU 0) for initial boot.
    ///
    /// Initializes x86 reset state, then sets:
    /// - Run state = RUN
    /// - RIP = 0xFFF0 (reset vector offset within CS segment)
    ///
    /// After `VM_RESET_CPU`, CS base is 0xFFFF_0000, so the first
    /// instruction fetched is at physical address 0xFFFF_FFF0 — the
    /// standard x86 reset vector which should fall within the bootrom.
    pub fn setup_bsp(&self) -> std::io::Result<()> {
        self.reboot_state()?;
        self.set_run_state(VRS_RUN)?;
        self.set_reg(vm_reg_name::VM_REG_GUEST_RIP, 0xfff0)?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // Execution
    // ---------------------------------------------------------------

    /// Enter the guest with a specific entry command and return the
    /// parsed VM exit.
    ///
    /// This is the primary execution method. The caller constructs a
    /// [`VmEntry`] (usually `VmEntry::Run` for the first entry, then
    /// `VmEntry::InoutFulfill` or `VmEntry::MmioFulfill` after
    /// handling an exit) and this method enters the guest and returns
    /// when the next exit occurs.
    pub fn enter(
        &self,
        entry: &VmEntry,
        api_version: u32,
    ) -> std::io::Result<VmExit> {
        let mut raw_exit = vm_exit::default();
        let mut raw_entry = entry.to_raw(
            self.id,
            &mut raw_exit as *mut vm_exit,
        );

        let fd = self.hdl.as_raw_fd();
        // Safety: VM_RUN takes a vm_entry pointer. raw_exit is
        // stack-local and outlives the ioctl. raw_entry.exit_data
        // points to raw_exit.
        let res = unsafe {
            libc::ioctl(fd, ioctls::VM_RUN as _, &mut raw_entry)
        };
        if res == -1 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(VmExit::parse(&raw_exit, api_version))
    }
}
