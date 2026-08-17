// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bhyve kernel-emulated ACPI PM timer.
//!
//! The PM timer runs in the kernel, but userspace must tell the kernel
//! which PIO port to attach it to via the VM_PMTMR_LOCATE ioctl.

use std::sync::Arc;

use vmm_core::hdl::VmmHdl;

/// Standard ACPI PM base port (matches C bhyve's IO_PMTMR).
pub const PMBASE_DEFAULT: u16 = 0x400;

/// Offset of the PM timer register within the PMBASE region.
const PM_TMR_OFFSET: u16 = 0x08;

/// Bhyve kernel-emulated ACPI PM Timer.
pub struct BhyvePmTimer {
    hdl: Arc<VmmHdl>,
    port: u16,
}

impl BhyvePmTimer {
    /// Create a PM timer at the given PMBASE port.
    pub fn create(hdl: Arc<VmmHdl>, pmbase: u16) -> Arc<Self> {
        let port = pmbase + PM_TMR_OFFSET;
        Arc::new(Self { hdl, port })
    }

    /// Tell the kernel which PIO port the PM timer uses.
    ///
    /// Must be called before vCPUs start. The kernel will handle
    /// PIO reads to this port by returning the timer value directly,
    /// without exiting to userspace.
    pub fn attach(&self) -> std::io::Result<()> {
        // VM_PMTMR_LOCATE tells the kernel to intercept reads
        // to this port and return the ACPI PM timer value.
        let fd = std::os::fd::AsRawFd::as_raw_fd(&*self.hdl);
        let port = self.port as usize;
        let res = unsafe {
            libc::ioctl(fd, bhyve_api::ioctls::VM_PMTMR_LOCATE as _, port)
        };
        if res == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Get the PIO port this timer is configured for.
    pub fn port(&self) -> u16 {
        self.port
    }
}
