// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Kernel state export/import via the vmm-data interface.
//!
//! State travels as raw class blobs, tagged with the class and version
//! the source kernel used. The kernel does not convert between struct
//! versions, so the destination checks every blob against its own class
//! table before it writes any of them. See [`checks`].

use bhyve_api::{
    vdi_field_entry_v1, vdi_version_entry_v1, vm_reg_name, VDC_ATPIC,
    VDC_ATPIT, VDC_HPET, VDC_IOAPIC, VDC_LAPIC, VDC_MSR, VDC_PMU_AMD,
    VDC_PM_TIMER, VDC_RTC,
};
use slog;
use std::mem::size_of;
use std::sync::Arc;
use vmm_core::hdl::VmmHdl;
use vmm_core::vcpu::Vcpu;

use crate::codec::{
    DevicePayload, DeviceState, MigrateError, VcpuStatePayload,
};
use crate::xsave::{XsaveLayout, XsavePlan};

mod checks;
mod debugregs;
mod time;

pub use time::{export_time_data, import_time_data};

use checks::{
    check_device_state, parse_vcpu_regs, read_class_layouts, RegWrite,
};

/// System device descriptors for state export/import.
const SYSTEM_DEVICES: &[(&str, u16, u16)] = &[
    ("ioapic", VDC_IOAPIC, 1),
    ("atpit", VDC_ATPIT, 1),
    ("atpic", VDC_ATPIC, 1),
    ("hpet", VDC_HPET, 1),
    ("pm_timer", VDC_PM_TIMER, 1),
    ("rtc", VDC_RTC, 2),
];

/// All guest registers to export/import for each vCPU.
const VCPU_REGS: &[vm_reg_name] = &[
    vm_reg_name::VM_REG_GUEST_RAX,
    vm_reg_name::VM_REG_GUEST_RBX,
    vm_reg_name::VM_REG_GUEST_RCX,
    vm_reg_name::VM_REG_GUEST_RDX,
    vm_reg_name::VM_REG_GUEST_RSI,
    vm_reg_name::VM_REG_GUEST_RDI,
    vm_reg_name::VM_REG_GUEST_RBP,
    vm_reg_name::VM_REG_GUEST_RSP,
    vm_reg_name::VM_REG_GUEST_R8,
    vm_reg_name::VM_REG_GUEST_R9,
    vm_reg_name::VM_REG_GUEST_R10,
    vm_reg_name::VM_REG_GUEST_R11,
    vm_reg_name::VM_REG_GUEST_R12,
    vm_reg_name::VM_REG_GUEST_R13,
    vm_reg_name::VM_REG_GUEST_R14,
    vm_reg_name::VM_REG_GUEST_R15,
    vm_reg_name::VM_REG_GUEST_RIP,
    vm_reg_name::VM_REG_GUEST_RFLAGS,
    vm_reg_name::VM_REG_GUEST_CR0,
    vm_reg_name::VM_REG_GUEST_CR3,
    vm_reg_name::VM_REG_GUEST_CR2,
    vm_reg_name::VM_REG_GUEST_CR4,
    vm_reg_name::VM_REG_GUEST_DR7,
    vm_reg_name::VM_REG_GUEST_DR0,
    vm_reg_name::VM_REG_GUEST_DR1,
    vm_reg_name::VM_REG_GUEST_DR2,
    vm_reg_name::VM_REG_GUEST_DR3,
    vm_reg_name::VM_REG_GUEST_DR6,
    vm_reg_name::VM_REG_GUEST_EFER,
    vm_reg_name::VM_REG_GUEST_XCR0,
    vm_reg_name::VM_REG_GUEST_CS,
    vm_reg_name::VM_REG_GUEST_DS,
    vm_reg_name::VM_REG_GUEST_ES,
    vm_reg_name::VM_REG_GUEST_FS,
    vm_reg_name::VM_REG_GUEST_GS,
    vm_reg_name::VM_REG_GUEST_SS,
    vm_reg_name::VM_REG_GUEST_TR,
    vm_reg_name::VM_REG_GUEST_LDTR,
];

/// Segment descriptor registers that use VM_GET/SET_SEGMENT_DESCRIPTOR
/// instead of VM_GET/SET_REGISTER.
const VCPU_SEG_DESCS: &[vm_reg_name] = &[
    vm_reg_name::VM_REG_GUEST_CS,
    vm_reg_name::VM_REG_GUEST_DS,
    vm_reg_name::VM_REG_GUEST_ES,
    vm_reg_name::VM_REG_GUEST_FS,
    vm_reg_name::VM_REG_GUEST_GS,
    vm_reg_name::VM_REG_GUEST_SS,
    vm_reg_name::VM_REG_GUEST_TR,
    vm_reg_name::VM_REG_GUEST_LDTR,
    vm_reg_name::VM_REG_GUEST_GDTR,
    vm_reg_name::VM_REG_GUEST_IDTR,
];

/// Export all registers for a vCPU as (reg_id, value) pairs,
/// plus segment descriptors as (reg_id, base, limit, access) tuples.
///
/// The interrupt shadow goes last, and only to a current peer: an older
/// one writes every register it reads and fails on a shadow the kernel
/// cannot set.
fn export_vcpu_regs(
    vcpu: &Vcpu,
    intr_shadow: bool,
) -> Result<Vec<u8>, MigrateError> {
    let mut data = Vec::with_capacity(
        (VCPU_REGS.len() + 1) * 12 + VCPU_SEG_DESCS.len() * 20,
    );
    let shadow = intr_shadow.then_some(vm_reg_name::VM_REG_GUEST_INTR_SHADOW);

    // General registers: 4 bytes id + 8 bytes value = 12 bytes each
    for &reg in VCPU_REGS.iter().chain(shadow.iter()) {
        let val = vcpu.get_reg(reg).map_err(|e| {
            MigrateError::VmmData(format!("get_reg {:?}: {e}", reg))
        })?;
        data.extend_from_slice(&(reg as u32).to_le_bytes());
        data.extend_from_slice(&val.to_le_bytes());
    }

    // Segment descriptors: 4 bytes id + 8 bytes base + 4 bytes limit + 4 bytes access = 20 bytes each
    // Use a marker to distinguish from regular registers
    let seg_marker = 0xFFFF_FFFFu32;
    data.extend_from_slice(&seg_marker.to_le_bytes());
    data.extend_from_slice(&(VCPU_SEG_DESCS.len() as u64).to_le_bytes());

    for &reg in VCPU_SEG_DESCS {
        let desc = vcpu.get_segment_desc(reg).map_err(|e| {
            MigrateError::VmmData(format!("get_segment_desc {:?}: {e}", reg))
        })?;
        data.extend_from_slice(&(reg as u32).to_le_bytes());
        data.extend_from_slice(&desc.base.to_le_bytes());
        data.extend_from_slice(&desc.limit.to_le_bytes());
        data.extend_from_slice(&desc.access.to_le_bytes());
    }

    Ok(data)
}

/// Run a VM-wide vmm-data transfer, which takes the VM write lock, in
/// the ring gate.
///
/// Under the migration's VM pause the rings are held stopped already,
/// and this stops nothing again.
fn with_vm_data<T>(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
    what: &str,
    xfer: impl FnOnce(bhyve_api::VmmDataOp<'_>) -> bhyve_api::VmmDataResult<T>,
) -> Result<T, MigrateError> {
    let fail = |e: String| {
        MigrateError::VmmData(format!(
            "{what} class={class} version={version}: {e}"
        ))
    };
    hdl.rings()
        .run(|rings| {
            let op = hdl
                .vm_data_op(rings, class, version)
                .map_err(|e| fail(e.to_string()))?;
            xfer(op).map_err(|e| fail(format!("{e:?}")))
        })
        .map_err(|e| fail(e.to_string()))?
}

/// Read a vmm-data class/version as raw bytes (for system devices).
fn read_class_all(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
) -> Result<Vec<u8>, MigrateError> {
    with_vm_data(hdl, class, version, "read_all", |op| op.read_all::<u8>())
}

/// Read a per-vCPU vmm-data class as raw bytes.
fn read_class_vcpu(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
    vcpuid: i32,
) -> Result<Vec<u8>, MigrateError> {
    let fail = |e: String| {
        MigrateError::VmmData(format!(
            "read_vcpu class={class} version={version} vcpu={vcpuid}: {e}"
        ))
    };
    hdl.vcpu_data_op(vcpuid, class, version)
        .map_err(|e| fail(e.to_string()))?
        .read_all::<u8>()
        .map_err(|e| fail(format!("{e:?}")))
}

/// Write raw bytes to a vmm-data class/version.
fn write_class_all(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
    data: &[u8],
) -> Result<(), MigrateError> {
    with_vm_data(hdl, class, version, "write", |op| op.write_many(data))
}

/// The kernel writes that a state import performs.
///
/// Only [`KernelSink`] is used in production. Tests use a recording
/// implementation, which is the only way to drive the import decisions
/// without a live bhyve instance.
trait ImportSink {
    fn write_class(
        &self,
        class: u16,
        version: u16,
        vcpuid: Option<i32>,
        data: &[u8],
    ) -> Result<(), MigrateError>;

    /// Segment selectors are not written here. The plain register
    /// list already carries CS through LDTR. The kernel's error is
    /// kept whole, because one errno is not a failure.
    fn write_reg(&self, vcpuid: i32, write: RegWrite) -> std::io::Result<()>;

    fn set_run_state(
        &self,
        vcpuid: i32,
        run_state: u32,
        sipi_vector: u8,
    ) -> Result<(), MigrateError>;

    fn set_fpu(&self, vcpuid: i32, area: &[u8]) -> Result<(), MigrateError>;
}

/// Applies imported state to a live bhyve instance.
struct KernelSink<'a> {
    hdl: &'a Arc<VmmHdl>,
}

impl KernelSink<'_> {
    fn vcpu(&self, vcpuid: i32) -> Vcpu {
        Vcpu::new_for_thread(vcpuid, Arc::clone(self.hdl))
    }
}

impl ImportSink for KernelSink<'_> {
    fn write_class(
        &self,
        class: u16,
        version: u16,
        vcpuid: Option<i32>,
        data: &[u8],
    ) -> Result<(), MigrateError> {
        let Some(id) = vcpuid else {
            return write_class_all(self.hdl, class, version, data)
                .map_err(|e| MigrateError::State(e.to_string()));
        };
        let fail = |e: String| {
            MigrateError::State(format!(
                "write class={class} version={version} vcpu={id}: {e}"
            ))
        };
        self.hdl
            .vcpu_data_op(id, class, version)
            .map_err(|e| fail(e.to_string()))?
            .write_many(data)
            .map_err(|e| fail(format!("{e:?}")))
    }

    fn write_reg(&self, vcpuid: i32, write: RegWrite) -> std::io::Result<()> {
        let vcpu = self.vcpu(vcpuid);
        match write {
            RegWrite::Reg(reg, value) => vcpu.set_reg(reg, value),
            RegWrite::Seg(reg, base, limit, access) => {
                let desc = bhyve_api::seg_desc {
                    base,
                    limit,
                    access,
                };
                vcpu.set_segment_desc(reg, &desc)
            }
        }
    }

    fn set_run_state(
        &self,
        vcpuid: i32,
        run_state: u32,
        sipi_vector: u8,
    ) -> Result<(), MigrateError> {
        self.vcpu(vcpuid)
            .set_run_state_full(run_state, sipi_vector)
            .map_err(|e| {
                MigrateError::VmmData(format!(
                    "set_run_state vcpu={vcpuid} state={run_state:#x}: {e}"
                ))
            })
    }

    fn set_fpu(&self, vcpuid: i32, area: &[u8]) -> Result<(), MigrateError> {
        self.vcpu(vcpuid).set_fpu(area).map_err(|e| {
            MigrateError::State(format!("vcpu {vcpuid}: set FPU state: {e}"))
        })
    }
}

/// Read a per-vCPU field-entry class and serialize it for the wire.
fn read_field_entries(
    hdl: &VmmHdl,
    class: u16,
    vcpuid: i32,
    what: &str,
) -> Result<Vec<u8>, MigrateError> {
    let fail = |e: String| {
        MigrateError::State(format!("export {what} vcpu={vcpuid}: {e}"))
    };
    let entries = hdl
        .vcpu_data_op(vcpuid, class, 1)
        .map_err(|e| fail(e.to_string()))?
        .read_all::<vdi_field_entry_v1>()
        .map_err(|e| fail(format!("{e:?}")))?;
    let mut buf =
        Vec::with_capacity(entries.len() * size_of::<vdi_field_entry_v1>());
    for entry in &entries {
        buf.extend_from_slice(&entry.vfe_ident.to_le_bytes());
        buf.extend_from_slice(&entry._pad.to_le_bytes());
        buf.extend_from_slice(&entry.vfe_value.to_le_bytes());
    }
    Ok(buf)
}

/// Export all kernel device state (per-vCPU + system devices).
///
/// The VM must be paused before calling this to get a consistent
/// snapshot. Uses individual register get/set for vCPU state
/// (more reliable than the vmm-data bulk interface for field-based
/// classes like VDC_REGISTER and VDC_MSR).
pub fn export_device_state(
    hdl: &Arc<VmmHdl>,
    num_cpus: u32,
    xsave: &XsaveLayout,
    current_peer: bool,
    log: &slog::Logger,
) -> Result<DeviceState, MigrateError> {
    let pmu_amd = current_peer && carries_pmu_amd(hdl)?;
    // Export per-vCPU state using individual register reads
    let mut vcpus = Vec::with_capacity(num_cpus as usize);
    for vcpuid in 0..num_cpus as i32 {
        let vcpu = Vcpu::new_for_thread(vcpuid, Arc::clone(hdl));

        // Registers via get_reg (reliable, avoids vmm-data field format issues)
        let registers = export_vcpu_regs(&vcpu, current_peer)?;

        // Every class below is required. A snapshot that silently
        // drops one resumes the guest with default state for it.
        let lapic = read_class_vcpu(hdl, VDC_LAPIC, 1, vcpuid)?;
        let msrs = read_field_entries(hdl, VDC_MSR, vcpuid, "MSR")?;
        let vmm_arch = read_field_entries(
            hdl,
            bhyve_api::VDC_VMM_ARCH,
            vcpuid,
            "VMM_ARCH",
        )?;

        let pmu_amd = pmu_amd
            .then(|| read_class_vcpu(hdl, VDC_PMU_AMD, 1, vcpuid))
            .transpose()?;

        let fpu = vcpu.get_fpu(xsave.size()).map_err(|e| {
            MigrateError::VmmData(format!("get FPU vcpu={vcpuid}: {e}"))
        })?;

        // Run state restores an AP that is in HLT or INIT, not running.
        let (run_state, sipi_vector) = vcpu.get_run_state().map_err(|e| {
            MigrateError::VmmData(format!("get_run_state vcpu={vcpuid}: {e}"))
        })?;

        slog::debug!(log, "vcpu state exported";
            "vcpu" => vcpuid,
            "lapic_bytes" => lapic.len(),
            "msr_bytes" => msrs.len(),
            "vmm_arch_bytes" => vmm_arch.len(),
            "run_state" => format!("{run_state:#x}"),
        );

        vcpus.push(VcpuStatePayload {
            vcpuid,
            registers,
            msrs,
            fpu,
            lapic,
            vmm_arch,
            run_state,
            sipi_vector,
            pmu_amd,
        });
    }

    // System devices. A class that fails to read is a failed export:
    // the destination cannot tell a missing class from an absent one.
    let mut devices = Vec::with_capacity(SYSTEM_DEVICES.len());
    for &(name, class, version) in SYSTEM_DEVICES {
        let data = read_class_all(hdl, class, version).map_err(|e| {
            MigrateError::State(format!("export device {name}: {e}"))
        })?;
        devices.push(DevicePayload {
            name: name.to_string(),
            class,
            version,
            data,
        });
    }

    Ok(DeviceState {
        vcpus,
        devices,
        emulated: Vec::new(),
        hyperv: None,
        platform: Vec::new(),
    })
}

/// Whether this host's vCPUs have an AMD PMU state to carry.
///
/// The class is in every kernel's table from v18 on, Intel hosts
/// included, but only SVM keeps the state behind it.
pub fn carries_pmu_amd(hdl: &VmmHdl) -> Result<bool, MigrateError> {
    if !crate::cpu::host_is_amd() {
        return Ok(false);
    }
    Ok(read_class_layouts(hdl)?
        .iter()
        .any(|e| e.vve_class == VDC_PMU_AMD && e.vve_version == 1))
}

/// Import all kernel device state (per-vCPU + system devices).
///
/// Time data must already be imported. The VM must be paused.
pub fn import_device_state(
    hdl: &Arc<VmmHdl>,
    state: &DeviceState,
    num_cpus: u32,
    xsave: &XsavePlan<'_>,
    current_peer: bool,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    let layouts = read_class_layouts(hdl)?;
    import_device_state_into(
        &KernelSink { hdl },
        &layouts,
        state,
        num_cpus,
        xsave,
        (crate::cpu::host_is_amd(), current_peer),
        log,
    )
}

/// Check the payload against the local kernel, then apply all of it.
///
/// Every write is fatal on failure. The caller aborts the migration and
/// the source VM stays alive.
fn import_device_state_into(
    sink: &dyn ImportSink,
    layouts: &[vdi_version_entry_v1],
    state: &DeviceState,
    num_cpus: u32,
    xsave: &XsavePlan<'_>,
    (amd_host, current_peer): (bool, bool),
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    check_device_state(state, layouts, num_cpus, amd_host, current_peer)?;
    let fpu: Vec<Vec<u8>> = state
        .vcpus
        .iter()
        .map(|v| xsave.translate(v.vcpuid, &v.fpu))
        .collect::<Result<_, _>>()
        .map_err(MigrateError::State)?;

    // System devices first, in the local order: the IOAPIC must be
    // ready before the LAPICs, and the payload's order is the peer's
    // choice.
    for &(name, class, _) in SYSTEM_DEVICES {
        let Some(dev) = state.devices.iter().find(|d| d.class == class) else {
            return Err(MigrateError::State(format!(
                "{name}: class {class} state is missing from the payload"
            )));
        };
        sink.write_class(dev.class, dev.version, None, &dev.data)?;
    }

    // Per vCPU: MSRs before the registers for GS_BASE, KERNEL_GS_BASE,
    // STAR and LSTAR. LAPIC before the registers so timer state is
    // correct. VMM_ARCH last, because it carries pending interrupts.
    for (vcpu, fpu) in state.vcpus.iter().zip(&fpu) {
        let id = vcpu.vcpuid;
        sink.write_class(VDC_MSR, 1, Some(id), &vcpu.msrs)?;
        sink.write_class(VDC_LAPIC, 1, Some(id), &vcpu.lapic)?;
        if let Some(pmu) = &vcpu.pmu_amd {
            sink.write_class(VDC_PMU_AMD, 1, Some(id), pmu)?;
        }
        for write in parse_vcpu_regs(id, &vcpu.registers)? {
            let write = debugregs::normalize(write, amd_host);
            match (write, sink.write_reg(id, write)) {
                (_, Ok(())) => {}
                // VMX refuses to force a shadow on, with EINVAL. The
                // guest may then take an interrupt one instruction
                // early, which beats failing a migration after the
                // gate's go. SVM takes it.
                (
                    RegWrite::Reg(vm_reg_name::VM_REG_GUEST_INTR_SHADOW, 1),
                    Err(e),
                ) if !amd_host
                    && e.kind() == std::io::ErrorKind::InvalidInput =>
                {
                    slog::warn!(log, "the interrupt shadow was not \
                        restored"; "vcpu" => id, "error" => %e);
                }
                (write, Err(e)) => {
                    return Err(MigrateError::State(format!(
                        "vcpu {id}: set {write:?}: {e}"
                    )));
                }
            }
        }
        sink.set_fpu(id, fpu)?;
        // VRS_HALT is 0, so a halted vCPU must be restored too.
        sink.set_run_state(id, vcpu.run_state, vcpu.sipi_vector)?;
        sink.write_class(bhyve_api::VDC_VMM_ARCH, 1, Some(id), &vcpu.vmm_arch)?;
        slog::debug!(log, "vcpu state imported";
            "vcpu" => id,
            "msr_bytes" => vcpu.msrs.len(),
            "run_state" => format!("{:#x}", vcpu.run_state),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::checks::fixtures::*;
    use super::*;
    use std::cell::RefCell;

    /// Records what an import applied, so the import decisions can be
    /// checked without a live bhyve instance.
    #[derive(Default)]
    struct RecordingSink {
        classes: RefCell<Vec<(u16, u16, Option<i32>, usize)>>,
        regs: RefCell<Vec<(i32, String)>>,
        run_states: RefCell<Vec<(i32, u32, u8)>>,
        fpus: RefCell<Vec<(i32, usize)>>,
        /// Answer a forced interrupt shadow the way VMX does.
        vmx: bool,
        /// A forced interrupt shadow fails with this instead.
        shadow_fails_with: Option<std::io::ErrorKind>,
    }

    impl RecordingSink {
        fn wrote_class(&self, class: u16, vcpuid: Option<i32>) -> bool {
            self.classes
                .borrow()
                .iter()
                .any(|&(c, _, v, _)| c == class && v == vcpuid)
        }

        fn applied_nothing(&self) -> bool {
            self.classes.borrow().is_empty()
                && self.regs.borrow().is_empty()
                && self.run_states.borrow().is_empty()
                && self.fpus.borrow().is_empty()
        }
    }

    impl ImportSink for RecordingSink {
        fn write_class(
            &self,
            class: u16,
            version: u16,
            vcpuid: Option<i32>,
            data: &[u8],
        ) -> Result<(), MigrateError> {
            self.classes.borrow_mut().push((
                class,
                version,
                vcpuid,
                data.len(),
            ));
            Ok(())
        }

        fn write_reg(
            &self,
            vcpuid: i32,
            write: RegWrite,
        ) -> std::io::Result<()> {
            if matches!(
                write,
                RegWrite::Reg(vm_reg_name::VM_REG_GUEST_INTR_SHADOW, v)
                    if v != 0
            ) {
                if let Some(kind) = self.shadow_fails_with {
                    return Err(kind.into());
                }
                if self.vmx {
                    return Err(std::io::ErrorKind::InvalidInput.into());
                }
            }
            self.regs.borrow_mut().push((vcpuid, format!("{write:?}")));
            Ok(())
        }

        fn set_run_state(
            &self,
            vcpuid: i32,
            run_state: u32,
            sipi_vector: u8,
        ) -> Result<(), MigrateError> {
            self.run_states
                .borrow_mut()
                .push((vcpuid, run_state, sipi_vector));
            Ok(())
        }

        fn set_fpu(
            &self,
            vcpuid: i32,
            area: &[u8],
        ) -> Result<(), MigrateError> {
            self.fpus.borrow_mut().push((vcpuid, area.len()));
            Ok(())
        }
    }

    fn plan(xsave: &XsaveLayout) -> XsavePlan<'_> {
        XsavePlan {
            source: xsave,
            local: xsave,
            guest: 0x3,
        }
    }

    fn test_log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    fn import(
        state: &DeviceState,
    ) -> (RecordingSink, Result<(), MigrateError>) {
        import_with_layouts(state, &kernel_layouts())
    }

    fn import_with_num_cpus(
        state: &DeviceState,
        num_cpus: u32,
    ) -> (RecordingSink, Result<(), MigrateError>) {
        let sink = RecordingSink::default();
        let xsave = test_xsave();
        let result = import_device_state_into(
            &sink,
            &kernel_layouts(),
            state,
            num_cpus,
            &plan(&xsave),
            (false, true),
            &test_log(),
        );
        (sink, result)
    }

    fn import_with_layouts(
        state: &DeviceState,
        layouts: &[vdi_version_entry_v1],
    ) -> (RecordingSink, Result<(), MigrateError>) {
        let sink = RecordingSink::default();
        let num_cpus = state.vcpus.len() as u32;
        let xsave = test_xsave();
        let result = import_device_state_into(
            &sink,
            layouts,
            state,
            num_cpus,
            &plan(&xsave),
            (false, true),
            &test_log(),
        );
        (sink, result)
    }

    fn import_on(
        sink: RecordingSink,
        state: &DeviceState,
        amd_host: bool,
    ) -> (RecordingSink, Result<(), MigrateError>) {
        let xsave = test_xsave();
        let result = import_device_state_into(
            &sink,
            &kernel_layouts(),
            state,
            state.vcpus.len() as u32,
            &plan(&xsave),
            (amd_host, true),
            &test_log(),
        );
        (sink, result)
    }

    #[test]
    fn the_debug_registers_and_the_shadow_are_carried() {
        for reg in [
            vm_reg_name::VM_REG_GUEST_DR0,
            vm_reg_name::VM_REG_GUEST_DR1,
            vm_reg_name::VM_REG_GUEST_DR2,
            vm_reg_name::VM_REG_GUEST_DR3,
            vm_reg_name::VM_REG_GUEST_DR6,
            vm_reg_name::VM_REG_GUEST_DR7,
        ] {
            assert!(
                VCPU_REGS.iter().any(|&r| r as i32 == reg as i32),
                "{reg:?}"
            );
        }
        // Sent apart, and last, so an older peer never sees it.
        let shadow = vm_reg_name::VM_REG_GUEST_INTR_SHADOW as i32;
        assert!(!VCPU_REGS.iter().any(|&r| r as i32 == shadow));
    }

    #[test]
    fn a_shadow_vmx_cannot_force_does_not_fail_the_import() {
        let mut state = good_state();
        state.vcpus[0].registers = full_reg_blob(1);
        let sink = RecordingSink {
            vmx: true,
            ..RecordingSink::default()
        };
        let (sink, result) = import_on(sink, &state, false);
        result.expect("a lost shadow is a warning on VMX");
        assert_eq!(sink.regs.borrow().len(), full_reg_count() - 1);
        assert!(sink.wrote_class(bhyve_api::VDC_VMM_ARCH, Some(0)));

        // On SVM a refused shadow is a real failure.
        let sink = RecordingSink {
            vmx: true,
            ..RecordingSink::default()
        };
        let (_, result) = import_on(sink, &state, true);
        result.map(|_| ()).expect_err("SVM takes a shadow");

        // Clearing it is always taken.
        state.vcpus[0].registers = full_reg_blob(0);
        let sink = RecordingSink {
            vmx: true,
            ..RecordingSink::default()
        };
        let (sink, result) = import_on(sink, &state, false);
        result.expect("cleared");
        assert_eq!(sink.regs.borrow().len(), full_reg_count());
    }

    #[test]
    fn only_einval_on_the_shadow_is_forgiven() {
        // Any other errno is a real failure, even on VMX.
        let mut state = good_state();
        state.vcpus[0].registers = full_reg_blob(1);
        let sink = RecordingSink {
            vmx: true,
            shadow_fails_with: Some(std::io::ErrorKind::PermissionDenied),
            ..RecordingSink::default()
        };
        let (_, result) = import_on(sink, &state, false);
        result
            .map(|_| ())
            .expect_err("EPERM is not the VMX refusal");
    }

    /// A register blob from a current source with `over` in place of
    /// the default values.
    fn blob_with(over: &[(vm_reg_name, u64)]) -> Vec<u8> {
        let regs: Vec<(vm_reg_name, u64)> = VCPU_REGS
            .iter()
            .map(|&r| {
                let v = over
                    .iter()
                    .find(|(o, _)| *o as i32 == r as i32)
                    .map_or(0, |&(_, v)| v);
                (r, v)
            })
            .chain([(vm_reg_name::VM_REG_GUEST_INTR_SHADOW, 0)])
            .collect();
        reg_blob(&regs, VCPU_SEG_DESCS)
    }

    #[test]
    fn hostile_debug_registers_are_refused_before_anything_is_applied() {
        for (reg, value) in [
            (vm_reg_name::VM_REG_GUEST_DR6, 0x1_FFFF_0FF0),
            (vm_reg_name::VM_REG_GUEST_DR7, 0x8000_0000_0000_0400),
            (vm_reg_name::VM_REG_GUEST_DR0, 0x0000_8000_0000_0000),
            (vm_reg_name::VM_REG_GUEST_DR2, 0xDEAD_BEEF_0000_0000),
        ] {
            for amd_host in [false, true] {
                let mut state = good_state();
                state.vcpus[0].registers = blob_with(&[(reg, value)]);
                let (sink, result) =
                    import_on(RecordingSink::default(), &state, amd_host);
                let error = result.map(|_| ()).expect_err("hostile DR");
                assert!(error.to_string().contains("cannot load"), "{error}");
                assert!(sink.applied_nothing(), "{reg:?}");
            }
        }
    }

    #[test]
    fn imported_debug_registers_carry_their_architectural_bits() {
        let mut state = good_state();
        state.vcpus[0].registers = blob_with(&[
            (vm_reg_name::VM_REG_GUEST_DR6, 0),
            (vm_reg_name::VM_REG_GUEST_DR7, 0xF800),
        ]);
        let (sink, result) = import_on(RecordingSink::default(), &state, true);
        result.expect("low reserved bits are fixed, not refused");
        let regs = sink.regs.borrow();
        let wrote = |want: &str| regs.iter().any(|(_, w)| w == want);
        assert!(wrote("Reg(VM_REG_GUEST_DR6, 4294838256)"), "{regs:?}");
        // Bit 13 (GD) stays; 11, 12, 14 and 15 clear on AMD; 10 sets.
        assert!(wrote("Reg(VM_REG_GUEST_DR7, 9216)"), "{regs:?}");
    }

    #[test]
    fn amd_pmu_state_goes_only_to_an_amd_host() {
        let mut state = good_state();
        state.vcpus[0].pmu_amd = Some(vec![0u8; PMU_AMD_LEN]);

        let (sink, result) = import_on(RecordingSink::default(), &state, true);
        result.expect("AMD to AMD");
        assert!(sink.wrote_class(VDC_PMU_AMD, Some(0)));

        let (sink, result) = import_on(RecordingSink::default(), &state, false);
        let error = result.map(|_| ()).expect_err("AMD state on Intel");
        assert!(error.to_string().contains("not AMD"), "{error}");
        assert!(sink.applied_nothing());

        state.vcpus[0].pmu_amd = Some(vec![0u8; PMU_AMD_LEN - 1]);
        let (sink, result) = import_on(RecordingSink::default(), &state, true);
        result.map(|_| ()).expect_err("short PMU blob");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn amd_pmu_state_is_for_every_vcpu_or_none() {
        let mut state = good_state();
        state.vcpus.push(vcpu_payload(1));
        state.vcpus[1].pmu_amd = Some(vec![0u8; PMU_AMD_LEN]);
        let (sink, result) = import_on(RecordingSink::default(), &state, true);
        let error = result.map(|_| ()).expect_err("one of two");
        assert!(error.to_string().contains("1 of 2"), "{error}");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn the_vcpu_payload_count_must_equal_the_vm_cpu_count() {
        // The kernel takes any id below VM_MAXCPU, so a payload for a
        // vCPU this VM never activated writes state into one with no
        // thread, and a missing one leaves that AP at reset.
        let mut state = good_state();
        state.vcpus.push(vcpu_payload(1));
        let (sink, result) = import_with_num_cpus(&state, 1);
        assert!(result.is_err(), "two payloads on a 1-CPU VM must fail");
        assert!(sink.applied_nothing());

        let (sink, result) = import_with_num_cpus(&good_state(), 2);
        assert!(result.is_err(), "one payload on a 2-CPU VM must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn a_vcpu_id_past_the_cpu_count_is_refused() {
        let mut state = good_state();
        state.vcpus = vec![vcpu_payload(17)];
        let (sink, result) = import_with_num_cpus(&state, 1);
        let error = result.map(|_| ()).expect_err("vcpu 17 on a 1-CPU VM");
        assert!(error.to_string().contains("outside 0..1"), "{error}");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn system_devices_are_non_empty() {
        assert!(!SYSTEM_DEVICES.is_empty());
    }

    #[test]
    fn system_device_classes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for &(_, class, _) in SYSTEM_DEVICES {
            assert!(
                seen.insert(class),
                "duplicate system device class: {class}"
            );
        }
    }

    #[test]
    fn well_formed_payload_imports() {
        let (sink, result) = import(&good_state());
        assert!(result.is_ok(), "{result:?}");
        assert!(sink.wrote_class(VDC_MSR, Some(0)));
        assert!(sink.wrote_class(VDC_LAPIC, Some(0)));
        assert!(sink.wrote_class(bhyve_api::VDC_VMM_ARCH, Some(0)));
        for &(_, class, _) in SYSTEM_DEVICES {
            assert!(sink.wrote_class(class, None), "class {class} not written");
        }
        assert_eq!(sink.regs.borrow().len(), full_reg_count());
        assert_eq!(sink.run_states.borrow().len(), 1);
        assert_eq!(sink.fpus.borrow().as_slice(), &[(0, TEST_XSAVE_LEN)]);
    }

    /// What a source that carries no FPU state sends. Resuming on the
    /// destination's own FPU state is OSF-2.
    #[test]
    fn a_payload_without_fpu_state_is_refused() {
        let mut state = good_state();
        state.vcpus[0].fpu.clear();
        let (sink, result) = import(&state);
        let error = result.map(|_| ()).expect_err("no FPU state");
        assert!(error.to_string().contains("vcpu 0 FPU"), "{error}");
        assert!(sink.applied_nothing(), "state applied before the check");
    }

    /// A bad area on the last vCPU must stop the import before the
    /// first vCPU is touched.
    #[test]
    fn a_bad_fpu_area_on_any_vcpu_applies_nothing() {
        let mut state = good_state();
        state.vcpus.push(vcpu_payload(1));
        state.vcpus[1].fpu[520] = 1;
        let (sink, result) = import_with_num_cpus(&state, 2);
        result.map(|_| ()).expect_err("compacted XSAVE header");
        assert!(sink.applied_nothing());
    }

    /// A destination kernel with a different `vdi_field_entry_v1` size
    /// leaves an MSR blob that is not a whole number of entries. That
    /// used to skip the write and report success, so the guest resumed
    /// with default MSRs.
    #[test]
    fn unaligned_msr_blob_fails_import() {
        let mut state = good_state();
        let mut msrs = field_entries(3);
        msrs.truncate(msrs.len() - 5);
        state.vcpus[0].msrs = msrs;

        let (sink, result) = import(&state);
        let err = result
            .map(|_| ())
            .expect_err("unaligned MSR blob must fail");
        let text = err.to_string();
        assert!(text.contains("MSR"), "{text}");
        assert!(sink.applied_nothing(), "state applied before the check");
    }

    /// The kernel's own element size is the authority, not ours.
    #[test]
    fn msr_entry_size_mismatch_fails_import() {
        let mut layouts = kernel_layouts();
        for e in layouts.iter_mut() {
            if e.vve_class == VDC_MSR {
                e.vve_len_per_item = 8;
            }
        }
        let (sink, result) = import_with_layouts(&good_state(), &layouts);
        assert!(result.is_err(), "entry size mismatch must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn missing_class_version_fails_import() {
        let mut layouts = kernel_layouts();
        for e in layouts.iter_mut() {
            if e.vve_class == VDC_LAPIC {
                e.vve_version = 2;
            }
        }
        let (sink, result) = import_with_layouts(&good_state(), &layouts);
        let err = result
            .map(|_| ())
            .expect_err("unknown LAPIC version must fail");
        assert!(err.to_string().contains("LAPIC"), "{err}");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn wrong_fixed_length_fails_import() {
        let mut state = good_state();
        state.devices[0].data.push(0);
        let (sink, result) = import(&state);
        assert!(result.is_err(), "wrong fixed length must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn unknown_device_class_fails_import() {
        let mut state = good_state();
        state.devices.push(DevicePayload {
            name: "mystery".to_string(),
            class: 0xFFFF,
            version: 1,
            data: vec![0u8; TEST_DEV_LEN],
        });
        let (sink, result) = import(&state);
        assert!(result.is_err(), "unknown device class must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn missing_system_device_fails_import() {
        let mut state = good_state();
        state.devices.pop();
        let (sink, result) = import(&state);
        assert!(result.is_err(), "missing device class must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn empty_lapic_fails_import() {
        let mut state = good_state();
        state.vcpus[0].lapic.clear();
        let (sink, result) = import(&state);
        assert!(result.is_err(), "missing LAPIC state must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn truncated_segment_descriptors_fail_import() {
        let mut state = good_state();
        let mut blob = default_reg_blob();
        blob.truncate(blob.len() - 4);
        state.vcpus[0].registers = blob;
        let (sink, result) = import(&state);
        assert!(result.is_err(), "truncated register blob must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn duplicate_vcpu_id_fails_import() {
        let mut state = good_state();
        state.vcpus.push(vcpu_payload(0));
        let (sink, result) = import(&state);
        assert!(result.is_err(), "duplicate vcpu id must fail");
        assert!(sink.applied_nothing());
    }

    /// VRS_HALT is 0. A halted vCPU must still have its run state
    /// restored, or it resumes in whatever state the fresh destination
    /// vCPU had.
    #[test]
    fn halted_run_state_is_applied() {
        let mut state = good_state();
        state.vcpus[0].run_state = bhyve_api::VRS_HALT;
        let (sink, result) = import(&state);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            sink.run_states.borrow().as_slice(),
            &[(0, bhyve_api::VRS_HALT, 0)]
        );
    }
}
