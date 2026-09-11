// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Safe wrapper over a bhyve VMM instance file descriptor.
//!
//! All kernel ioctls are exposed as typed, safe methods. The raw ioctl
//! interface is intentionally kept private — callers should use the
//! typed methods, and new operations should be added here rather than
//! exposed via a generic ioctl escape hatch.

use std::io::{Error, ErrorKind, Result};
use std::os::fd::AsRawFd;
use std::time::Duration;

use bhyve_api::{
    ioctls, vcpu_cpuid_entry, vm_capability, vm_create_req, vm_isa_irq,
    vm_lapic_msi, vm_memmap, vm_memseg, vm_npt_operation, vm_rtc_data,
    vm_suspend, vm_vcpu_cpuid_config, vmm_dirty_tracker, ApiVersion, VmmCtlFd,
    VmmFd, VCF_RESERVOIR_MEM, VCF_TRACK_DIRTY, VM_MAX_SEG_NAMELEN,
    VM_MEMMAP_F_WIRED, VNO_FLAG_BITMAP_IN, VNO_OP_SET_DIRTY,
};

use crate::mem::Prot;

/// Options for creating a new VM instance.
#[derive(Debug, Clone, Default)]
pub struct CreateOpts {
    /// Force creation even if a VM with this name already exists.
    pub force: bool,
    /// Allocate memory from the VMM reservoir.
    pub use_reservoir: bool,
    /// Enable dirty page tracking (required for live migration).
    pub track_dirty: bool,
}

/// Handle to a bhyve VMM instance.
///
/// Wraps the kernel `/dev/vmm/{name}` file descriptor and provides safe
/// typed methods for all VM operations. The raw ioctl interface is
/// private — all operations must go through the methods on this type.
pub struct VmmHdl {
    inner: VmmFd,
    name: String,
}

impl VmmHdl {
    /// Create a new VM instance and open a handle to it.
    pub fn create(name: &str, opts: &CreateOpts) -> Result<Self> {
        let ctl = VmmCtlFd::open()?;

        if opts.force {
            // Best-effort destroy of any existing VM with this name;
            // failure is expected if no VM exists
            let _ = ctl.vm_destroy(name.as_bytes());
        }

        let mut req = vm_create_req::new(name.as_bytes())?;
        if opts.use_reservoir {
            req.flags |= VCF_RESERVOIR_MEM;
        }
        if opts.track_dirty {
            req.flags |= VCF_TRACK_DIRTY;
        }

        let mut retries = 0;
        loop {
            // Safety: vm_create_req is the correct struct for VMM_CREATE_VM.
            match unsafe { ctl.ioctl(ioctls::VMM_CREATE_VM, &mut req) } {
                Ok(_) => break,
                Err(err)
                    if opts.force
                        && matches!(
                            err.raw_os_error(),
                            Some(libc::EEXIST) | Some(libc::EBUSY)
                        )
                        && retries < 20 =>
                {
                    // A destroyed VM can remain visible until kernel teardown
                    // releases its final reference.
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(err) => return Err(err),
            }
        }

        // VMM_CREATE_VM has succeeded, so a failed open leaves an instance
        // that no fd owns and no autodestruct can reclaim. Destroy it here.
        let fd = match VmmFd::open(name) {
            Ok(fd) => fd,
            Err(err) => {
                let _ = ctl.vm_destroy(name.as_bytes());
                return Err(err);
            }
        };
        Ok(Self {
            inner: fd,
            name: name.to_string(),
        })
    }

    /// Open an existing VM instance.
    pub fn open(name: &str) -> Result<Self> {
        let fd = VmmFd::open(name)?;
        Ok(Self {
            inner: fd,
            name: name.to_string(),
        })
    }

    /// Get the VM name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the bhyve API version from the kernel.
    pub fn api_version(&self) -> Result<u32> {
        self.inner.api_version()
    }

    // ---------------------------------------------------------------
    // Memory segment management
    // ---------------------------------------------------------------

    /// Allocate a guest memory segment in the kernel.
    ///
    /// Returns an error if the segment name exceeds the kernel's
    /// maximum length.
    pub fn create_memseg(
        &self,
        segid: i32,
        len: usize,
        name: &str,
    ) -> Result<()> {
        let name_bytes = name.as_bytes();
        if name_bytes.len() >= VM_MAX_SEG_NAMELEN {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "segment name '{}' exceeds max length {}",
                    name,
                    VM_MAX_SEG_NAMELEN - 1,
                ),
            ));
        }

        let mut seg = vm_memseg {
            segid,
            len,
            name: [0u8; VM_MAX_SEG_NAMELEN],
        };
        seg.name[..name_bytes.len()].copy_from_slice(name_bytes);

        // Safety: vm_memseg is the correct struct for VM_ALLOC_MEMSEG.
        unsafe { self.inner.ioctl(ioctls::VM_ALLOC_MEMSEG, &mut seg) }?;
        Ok(())
    }

    /// Map a memory segment into the guest physical address space.
    pub fn map_memseg(
        &self,
        segid: i32,
        gpa: u64,
        len: usize,
        segoff: i64,
        prot: Prot,
    ) -> Result<()> {
        let mut map = vm_memmap {
            gpa,
            segid,
            segoff,
            len,
            prot: prot.bits(),
            flags: VM_MEMMAP_F_WIRED,
        };
        // Safety: vm_memmap is the correct struct for VM_MMAP_MEMSEG.
        unsafe { self.inner.ioctl(ioctls::VM_MMAP_MEMSEG, &mut map) }?;
        Ok(())
    }

    /// Get the mmap offset for a device memory segment.
    pub fn devmem_offset(&self, segid: i32) -> Result<i64> {
        let mut dmo = bhyve_api::vm_devmem_offset { segid, offset: 0 };
        // Safety: vm_devmem_offset is the correct struct.
        unsafe { self.inner.ioctl(ioctls::VM_DEVMEM_GETOFFSET, &mut dmo) }?;
        Ok(dmo.offset)
    }

    /// Remove a memory segment mapping from the guest physical address space.
    ///
    /// The kernel matches an exact `(gpa, len)` pair, so the caller must pass
    /// the same values used for [`Self::map_memseg`].
    pub fn munmap_memseg(&self, gpa: u64, len: usize) -> Result<()> {
        let mut arg = bhyve_api::vm_munmap { gpa, len };
        // Safety: vm_munmap is the correct struct for VM_MUNMAP_MEMSEG.
        unsafe { self.inner.ioctl(ioctls::VM_MUNMAP_MEMSEG, &mut arg) }?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // RTC
    // ---------------------------------------------------------------

    /// Set the time reported by the virtual RTC.
    pub fn rtc_settime(&self, time: Duration) -> Result<()> {
        self.inner.rtc_settime(time)
    }

    /// Write a byte to the virtual RTC at the given offset.
    pub fn rtc_write(&self, offset: i32, value: u8) -> Result<()> {
        let mut data = vm_rtc_data { offset, value };
        // Safety: vm_rtc_data is the correct struct for VM_RTC_WRITE.
        unsafe { self.inner.ioctl(ioctls::VM_RTC_WRITE, &mut data) }?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // Interrupts
    // ---------------------------------------------------------------

    /// Assert an ISA IRQ (edge: assert + deassert).
    pub fn isa_assert_irq(
        &self,
        atpic_irq: i32,
        ioapic_irq: i32,
    ) -> Result<()> {
        self.isa_irq_op(ioctls::VM_ISA_ASSERT_IRQ, atpic_irq, ioapic_irq)
    }

    /// Deassert an ISA IRQ.
    pub fn isa_deassert_irq(
        &self,
        atpic_irq: i32,
        ioapic_irq: i32,
    ) -> Result<()> {
        self.isa_irq_op(ioctls::VM_ISA_DEASSERT_IRQ, atpic_irq, ioapic_irq)
    }

    /// Set an ISA IRQ trigger mode (edge or level).
    pub fn isa_set_irq_trigger(
        &self,
        irq: i32,
        level_trigger: bool,
    ) -> Result<()> {
        let mut trigger = bhyve_api::vm_isa_irq_trigger {
            atpic_irq: irq,
            trigger: if level_trigger { 1 } else { 0 },
        };
        // Safety: vm_isa_irq_trigger is the correct struct.
        unsafe {
            self.inner
                .ioctl(ioctls::VM_ISA_SET_IRQ_TRIGGER, &mut trigger)
        }?;
        Ok(())
    }

    /// Pulse an ISA IRQ (assert then immediately deassert).
    pub fn isa_pulse_irq(&self, atpic_irq: i32, ioapic_irq: i32) -> Result<()> {
        self.isa_irq_op(ioctls::VM_ISA_PULSE_IRQ, atpic_irq, ioapic_irq)
    }

    fn isa_irq_op(
        &self,
        cmd: i32,
        atpic_irq: i32,
        ioapic_irq: i32,
    ) -> Result<()> {
        let mut irq = vm_isa_irq {
            atpic_irq,
            ioapic_irq,
        };
        // Safety: vm_isa_irq is the correct struct for ISA IRQ ioctls.
        unsafe { self.inner.ioctl(cmd, &mut irq) }?;
        Ok(())
    }

    /// Assert an IOAPIC IRQ (for PCI device GSI delivery).
    pub fn ioapic_assert_irq(&self, irq: i32) -> Result<()> {
        let mut req = bhyve_api::vm_ioapic_irq { irq };
        unsafe { self.inner.ioctl(ioctls::VM_IOAPIC_ASSERT_IRQ, &mut req) }?;
        Ok(())
    }

    /// Deassert an IOAPIC IRQ.
    pub fn ioapic_deassert_irq(&self, irq: i32) -> Result<()> {
        let mut req = bhyve_api::vm_ioapic_irq { irq };
        unsafe { self.inner.ioctl(ioctls::VM_IOAPIC_DEASSERT_IRQ, &mut req) }?;
        Ok(())
    }

    /// Deliver a LAPIC MSI (message signaled interrupt).
    pub fn lapic_msi(&self, addr: u64, msg: u64) -> Result<()> {
        let mut msi = vm_lapic_msi { addr, msg };
        // Safety: vm_lapic_msi is the correct struct for VM_LAPIC_MSI.
        unsafe { self.inner.ioctl(ioctls::VM_LAPIC_MSI, &mut msi) }?;
        Ok(())
    }

    /// Inject an interrupt directly into a vCPU's LAPIC.
    ///
    /// Sets the IRR bit for the given vector and kicks the vCPU
    /// out of HLT via vcpu_notify_event(). More direct than
    /// lapic_msi() for targeted vCPU interrupt delivery.
    pub fn lapic_irq(&self, cpuid: i32, vector: i32) -> Result<()> {
        let mut irq = bhyve_api::vm_lapic_irq { cpuid, vector };
        unsafe { self.inner.ioctl(ioctls::VM_LAPIC_IRQ, &mut irq) }?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // VM capabilities and lifecycle
    // ---------------------------------------------------------------

    /// Set the CPU topology (sockets, cores, threads per core).
    pub fn set_topology(
        &self,
        sockets: u16,
        cores: u16,
        threads: u16,
        maxcpus: u16,
    ) -> Result<()> {
        // struct vm_cpu_topology { u16 sockets, cores, threads, maxcpus }
        let mut topo: [u16; 4] = [sockets, cores, threads, maxcpus];
        // Safety: passing a 4×u16 struct matching vm_cpu_topology.
        unsafe {
            self.inner.ioctl(ioctls::VM_SET_TOPOLOGY, topo.as_mut_ptr())
        }?;
        Ok(())
    }

    /// Set a VM capability (e.g., halt-on-exit, pause-on-exit).
    pub fn set_capability(
        &self,
        cpuid: i32,
        captype: i32,
        capval: i32,
    ) -> Result<()> {
        let mut cap = vm_capability {
            cpuid,
            captype,
            capval,
            allcpus: 0,
        };
        // Safety: vm_capability is the correct struct.
        unsafe { self.inner.ioctl(ioctls::VM_SET_CAPABILITY, &mut cap) }?;
        Ok(())
    }

    /// Pause the VM (all vCPUs and kernel timers).
    pub fn pause(&self) -> Result<()> {
        self.inner.ioctl_usize(ioctls::VM_PAUSE, 0)?;
        Ok(())
    }

    /// Resume the VM after pause.
    pub fn resume(&self) -> Result<()> {
        self.inner.ioctl_usize(ioctls::VM_RESUME, 0)?;
        Ok(())
    }

    /// Request VM destruction.
    pub fn destroy(&self) -> Result<()> {
        self.inner.ioctl_usize(ioctls::VM_DESTROY_SELF, 0)?;
        Ok(())
    }

    /// Arm or disarm destruction of the instance when the last handle
    /// closes.
    ///
    /// WHY the argument matters: the kernel sets the VMM_AUTODESTROY flag
    /// for a non-zero argument and CLEARS it for zero (`vmm_sol_dev.c`,
    /// `VM_SET_AUTODESTRUCT`). A call that always sent zero disarmed the
    /// only reclaimer that survives a crash or a SIGKILL.
    pub fn set_autodestruct(&self, enable: bool) -> Result<()> {
        self.inner
            .ioctl_usize(ioctls::VM_SET_AUTODESTRUCT, usize::from(enable))?;
        Ok(())
    }

    /// Suspend the VM (power-off, reset, halt, or triple-fault).
    pub fn suspend(&self, how: u32, source: i32) -> Result<()> {
        let mut req = vm_suspend { how, source };
        // Safety: vm_suspend is the correct struct for VM_SUSPEND.
        unsafe { self.inner.ioctl(ioctls::VM_SUSPEND, &mut req) }?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // Dirty page tracking (for live migration)
    // ---------------------------------------------------------------

    /// Track dirty pages for a memory region.
    ///
    /// Returns a bitmap where each bit represents a 4 KiB page.
    /// Calling this atomically reads and clears the dirty bits.
    ///
    /// The `bitmap` buffer must be large enough to hold one bit per
    /// page in the region: `ceil(len / 4096 / 8)` bytes.
    pub fn track_dirty_pages(
        &self,
        start_gpa: u64,
        len: usize,
        bitmap: &mut [u8],
    ) -> Result<()> {
        let pages = len.div_ceil(4096);
        let required_bytes = pages.div_ceil(8);
        if bitmap.len() < required_bytes {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "bitmap too small: need {} bytes for {} pages, got {}",
                    required_bytes,
                    pages,
                    bitmap.len(),
                ),
            ));
        }

        let mut tracker = vmm_dirty_tracker {
            vdt_start_gpa: start_gpa,
            vdt_len: len,
            // Safety: bitmap is a valid mutable slice that outlives this
            // ioctl call (we block until it completes).
            vdt_pfns: bitmap.as_mut_ptr() as *mut _,
        };
        // Safety: vmm_dirty_tracker is the correct struct.
        unsafe {
            self.inner.ioctl(ioctls::VM_TRACK_DIRTY_PAGES, &mut tracker)
        }?;
        Ok(())
    }

    /// Set pages as dirty in the NPT (for migration re-dirtying).
    ///
    /// Requires bhyve API version >= 17. Returns an error if the
    /// kernel does not support this operation.
    pub fn set_dirty_pages(
        &self,
        gpa: u64,
        len: u64,
        bitmap: &[u8],
    ) -> Result<()> {
        let api_ver = self.api_version()?;
        if api_ver < ApiVersion::V17 as u32 {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("set_dirty_pages requires API v17+, have v{}", api_ver,),
            ));
        }

        let mut op = vm_npt_operation {
            vno_gpa: gpa,
            vno_len: len,
            // Safety: bitmap is a valid slice that outlives this ioctl.
            // The const-to-mut cast is required by the kernel interface
            // but the kernel only reads from the bitmap when
            // VNO_FLAG_BITMAP_IN is set.
            vno_bitmap: bitmap.as_ptr() as *mut u8,
            vno_operation: VNO_OP_SET_DIRTY | VNO_FLAG_BITMAP_IN,
        };
        // Safety: vm_npt_operation is the correct struct.
        unsafe { self.inner.ioctl(ioctls::VM_NPT_OPERATION, &mut op) }?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // VMM data interface (state export/import for migration)
    // ---------------------------------------------------------------

    /// Build a vmm-data read/write operation.
    pub fn data_op(
        &self,
        class: u16,
        version: u16,
    ) -> bhyve_api::VmmDataOp<'_> {
        self.inner.data_op(class, version)
    }

    // ---------------------------------------------------------------
    // CPUID control
    // ---------------------------------------------------------------

    /// Set custom CPUID entries for a vCPU.
    ///
    /// The entries must be sorted by function/index (use
    /// `vcpu_cpuid_entry::eval_sort`). The kernel will use these
    /// entries to emulate CPUID for the guest instead of passing
    /// through host values.
    ///
    /// Requires bhyve API version >= 5.
    pub fn set_cpuid(
        &self,
        vcpuid: i32,
        entries: &mut [vcpu_cpuid_entry],
        flags: u32,
    ) -> Result<()> {
        let mut config = vm_vcpu_cpuid_config {
            vvcc_vcpuid: vcpuid,
            vvcc_flags: flags,
            vvcc_nent: entries.len() as u32,
            _pad: 0,
            vvcc_entries: entries.as_mut_ptr() as *mut std::ffi::c_void,
        };
        unsafe { self.inner.ioctl(ioctls::VM_SET_CPUID, &mut config) }?;
        Ok(())
    }
}

impl AsRawFd for VmmHdl {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.inner.as_raw_fd()
    }
}

// VmmHdl intentionally does NOT expose a generic ioctl method.
// All operations must go through typed methods above.
// To add new operations, add a new method here rather than
// using a raw ioctl escape hatch.
