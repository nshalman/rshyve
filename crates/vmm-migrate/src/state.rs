// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Kernel state export/import via the vmm-data interface.
//!
//! All device state is transferred as raw bytes with embedded version
//! information. The kernel handles struct versioning -- we just need
//! to read the blobs on the source and write them on the destination.

use bhyve_api::{
    VDC_ATPIC, VDC_ATPIT, VDC_HPET, VDC_IOAPIC,
    VDC_LAPIC, VDC_MSR, VDC_PM_TIMER, VDC_RTC, VDC_VMM_TIME,
    vdi_field_entry_v1, vm_reg_name, seg_desc,
};
use std::sync::Arc;
use vmm_core::hdl::VmmHdl;
use vmm_core::vcpu::Vcpu;

use crate::codec::{
    DevicePayload, DeviceState, MigrateError, TimeData, VcpuStatePayload,
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
fn export_vcpu_regs(vcpu: &Vcpu) -> Result<Vec<u8>, MigrateError> {
    let mut data = Vec::with_capacity(
        VCPU_REGS.len() * 12 + VCPU_SEG_DESCS.len() * 20,
    );

    // General registers: 4 bytes id + 8 bytes value = 12 bytes each
    for &reg in VCPU_REGS {
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

/// Import registers for a vCPU from (reg_id, value) pairs,
/// followed by segment descriptors after a marker.
fn import_vcpu_regs(vcpu: &Vcpu, data: &[u8]) -> Result<(), MigrateError> {
    let mut pos = 0;

    // Read general registers (12 bytes each: 4 id + 8 value)
    while pos + 12 <= data.len() {
        let reg_id = u32::from_le_bytes(
            data[pos..pos + 4].try_into().expect("4 bytes"),
        );

        // Check for segment descriptor marker
        if reg_id == 0xFFFF_FFFF {
            pos += 4;
            let count = u64::from_le_bytes(
                data[pos..pos + 8].try_into().expect("8 bytes"),
            ) as usize;
            pos += 8;

            // Read segment descriptors (20 bytes each: 4 id + 8 base + 4 limit + 4 access)
            for _ in 0..count {
                if pos + 20 > data.len() {
                    break;
                }
                let seg_id = u32::from_le_bytes(
                    data[pos..pos + 4].try_into().expect("4 bytes"),
                );
                let base = u64::from_le_bytes(
                    data[pos + 4..pos + 12].try_into().expect("8 bytes"),
                );
                let limit = u32::from_le_bytes(
                    data[pos + 12..pos + 16].try_into().expect("4 bytes"),
                );
                let access = u32::from_le_bytes(
                    data[pos + 16..pos + 20].try_into().expect("4 bytes"),
                );
                let reg: vm_reg_name = unsafe {
                    std::mem::transmute(seg_id as i32)
                };
                let desc = bhyve_api::seg_desc { base, limit, access };
                vcpu.set_segment_desc(reg, &desc).map_err(|e| {
                    MigrateError::VmmData(format!(
                        "set_segment_desc {:?}: {e}", reg
                    ))
                })?;
                // Don't touch selectors here — they're already set
                // by the general register import (VCPU_REGS includes CS-LDTR).
                pos += 20;
            }
            break;
        }

        let val = u64::from_le_bytes(
            data[pos + 4..pos + 12].try_into().expect("8 bytes"),
        );
        let reg: vm_reg_name = unsafe { std::mem::transmute(reg_id as i32) };
        if let Err(e) = vcpu.set_reg(reg, val) {
            // Some registers may fail on cross-host migration due to
            // CPU feature differences (e.g., XCR0 with AVX bits on a
            // host that doesn't support AVX). Log and continue.
            eprintln!("[migrate] set_reg {:?} = {:#x} failed: {e} (continuing)", reg, val);
        }
        pos += 12;
    }
    Ok(())
}

/// Read a vmm-data class/version as raw bytes (for system devices).
fn read_class_all(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
) -> Result<Vec<u8>, MigrateError> {
    hdl.data_op(class, version)
        .read_all::<u8>()
        .map_err(|e| MigrateError::VmmData(format!(
            "read_all class={class} version={version}: {e:?}"
        )))
}

/// Read a per-vCPU vmm-data class as raw bytes.
fn read_class_vcpu(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
    vcpuid: i32,
) -> Result<Vec<u8>, MigrateError> {
    hdl.data_op(class, version)
        .for_vcpu(vcpuid)
        .read_all::<u8>()
        .map_err(|e| MigrateError::VmmData(format!(
            "read_vcpu class={class} version={version} vcpu={vcpuid}: {e:?}"
        )))
}

/// Write raw bytes to a vmm-data class/version.
fn write_class_all(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
    data: &[u8],
) -> Result<(), MigrateError> {
    hdl.data_op(class, version)
        .write_many(data)
        .map_err(|e| MigrateError::VmmData(format!(
            "write class={class} version={version}: {e:?}"
        )))
}

/// Write per-vCPU vmm-data class.
fn write_class_vcpu(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
    vcpuid: i32,
    data: &[u8],
) -> Result<(), MigrateError> {
    hdl.data_op(class, version)
        .for_vcpu(vcpuid)
        .write_many(data)
        .map_err(|e| MigrateError::VmmData(format!(
            "write_vcpu class={class} version={version} vcpu={vcpuid}: {e:?}"
        )))
}

/// Export time synchronization data from the kernel.
pub fn export_time_data(hdl: &Arc<VmmHdl>) -> Result<TimeData, MigrateError> {
    let vmm_time = read_class_all(hdl, VDC_VMM_TIME, 1)?;
    eprintln!("[migrate] exported VMM_TIME: {} bytes", vmm_time.len());
    Ok(TimeData { vmm_time })
}

/// Import time synchronization data into the kernel.
///
/// This must be called before importing device state, because timers
/// need the correct time base.
pub fn import_time_data(
    hdl: &Arc<VmmHdl>,
    time: &TimeData,
) -> Result<(), MigrateError> {
    use bhyve_api::vdi_time_info_v1;

    // Parse source time data
    if time.vmm_time.len() < std::mem::size_of::<vdi_time_info_v1>() {
        return Err(MigrateError::VmmData("VMM_TIME too small".into()));
    }
    let src: vdi_time_info_v1 = unsafe {
        std::ptr::read(time.vmm_time.as_ptr() as *const vdi_time_info_v1)
    };

    // Read destination's current host time snapshot
    let dst = hdl.data_op(VDC_VMM_TIME, 1)
        .read::<vdi_time_info_v1>()
        .map_err(|e| MigrateError::VmmData(format!("read dst VMM_TIME: {e:?}")))?;

    // Adjust boot_hrtime for the destination host (Propolis pattern):
    //
    // guest_uptime = src.hrtime - src.boot_hrtime
    // migrate_delta = dst_wall_clock - src_wall_clock  (clamped to 0)
    // new_boot_hrtime = dst.hrtime - (guest_uptime + migrate_delta)
    // new_guest_tsc = src.guest_tsc + tsc_ticks_during_migration
    let guest_uptime = src.vt_hrtime - src.vt_boot_hrtime;

    let src_wc_ns = (src.vt_hres_sec as i64) * 1_000_000_000 + (src.vt_hres_ns as i64);
    let dst_wc_ns = (dst.vt_hres_sec as i64) * 1_000_000_000 + (dst.vt_hres_ns as i64);
    let migrate_delta_ns = (dst_wc_ns - src_wc_ns).max(0);

    let new_boot_hrtime = dst.vt_hrtime - (guest_uptime + migrate_delta_ns);

    // Adjust guest TSC forward by migration duration.
    // Use destination's freq for the TSC delta calculation when freqs differ,
    // since the guest TSC will tick at the destination's rate.
    let tsc_freq_for_delta = if dst.vt_guest_freq != src.vt_guest_freq {
        dst.vt_guest_freq
    } else {
        src.vt_guest_freq
    };
    let tsc_delta = if tsc_freq_for_delta > 0 {
        (migrate_delta_ns as u64) * tsc_freq_for_delta / 1_000_000_000
    } else {
        0
    };
    // Scale the source TSC to the destination's frequency
    let scaled_guest_tsc = if dst.vt_guest_freq != src.vt_guest_freq && src.vt_guest_freq > 0 {
        (src.vt_guest_tsc as u128 * dst.vt_guest_freq as u128 / src.vt_guest_freq as u128) as u64
    } else {
        src.vt_guest_tsc
    };
    let new_guest_tsc = scaled_guest_tsc.wrapping_add(tsc_delta);

    eprintln!(
        "[migrate] VMM_TIME adjust: guest_uptime={}ms migrate_delta={}ms new_boot_hrtime={} tsc_delta={}",
        guest_uptime / 1_000_000,
        migrate_delta_ns / 1_000_000,
        new_boot_hrtime,
        tsc_delta,
    );

    // Use destination's guest_freq if it differs from source.
    // The kernel may reject writes with a mismatched frequency
    // (EPERM). TSC frequency scaling is handled by the kernel.
    let guest_freq = if dst.vt_guest_freq != src.vt_guest_freq {
        eprintln!(
            "[migrate] TSC freq mismatch: src={}Hz dst={}Hz, using dst",
            src.vt_guest_freq, dst.vt_guest_freq,
        );
        dst.vt_guest_freq
    } else {
        src.vt_guest_freq
    };

    let adjusted = vdi_time_info_v1 {
        vt_guest_freq: guest_freq,
        vt_guest_tsc: new_guest_tsc,
        vt_boot_hrtime: new_boot_hrtime,
        vt_hrtime: dst.vt_hrtime,
        vt_hres_sec: dst.vt_hres_sec,
        vt_hres_ns: dst.vt_hres_ns,
    };

    // Write adjusted time data
    let adjusted_bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(
            &adjusted as *const _ as *const u8,
            std::mem::size_of::<vdi_time_info_v1>(),
        )
    };
    write_class_all(hdl, VDC_VMM_TIME, 1, adjusted_bytes)?;
    eprintln!("[migrate] VMM_TIME import OK (adjusted)");
    Ok(())
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
) -> Result<DeviceState, MigrateError> {
    // Export per-vCPU state using individual register reads
    let mut vcpus = Vec::with_capacity(num_cpus as usize);
    for vcpuid in 0..num_cpus as i32 {
        let vcpu = Vcpu::new_for_thread(vcpuid, Arc::clone(hdl));

        // Registers via get_reg (reliable, avoids vmm-data field format issues)
        let registers = export_vcpu_regs(&vcpu)?;

        // LAPIC: per-vCPU struct-based, works with read_all
        let lapic = match read_class_vcpu(hdl, VDC_LAPIC, 1, vcpuid) {
            Ok(data) => {
                // Dump timer fields at correct offsets
                if data.len() >= 192 {
                    let lvt_timer = u32::from_le_bytes(data[144..148].try_into().unwrap_or([0;4]));
                    let icr_timer = u32::from_le_bytes(data[168..172].try_into().unwrap_or([0;4]));
                    let timer_target = i64::from_le_bytes(data[184..192].try_into().unwrap_or([0;8]));
                    eprintln!("[migrate] LAPIC vcpu {vcpuid}: lvt_timer={lvt_timer:#x} icr_timer={icr_timer} timer_target={timer_target}");
                }
                // Dump vl_timer_target (i64 at offset 168 in vdi_lapic_v1)
                if data.len() >= 176 {
                    let timer_target = i64::from_le_bytes(
                        data[168..176].try_into().unwrap_or([0;8])
                    );
                    let icr_timer = u32::from_le_bytes(
                        data[152..156].try_into().unwrap_or([0;4])
                    );
                    let lvt_timer = u32::from_le_bytes(
                        data[128..132].try_into().unwrap_or([0;4])
                    );
                    eprintln!("[migrate] LAPIC vcpu {vcpuid}: timer_target={timer_target} icr_timer={icr_timer} lvt_timer={lvt_timer:#x}");
                }
                data
            }
            Err(e) => {
                eprintln!("[migrate] warning: LAPIC export vcpu {vcpuid}: {e}");
                Vec::new()
            }
        };

        // MSRs: bulk read via VDC_MSR returns vdi_field_entry_v1 array
        let msrs = match hdl.data_op(VDC_MSR, 1)
            .for_vcpu(vcpuid)
            .read_all::<vdi_field_entry_v1>()
        {
            Ok(entries) => {
                eprintln!("[migrate] exported {} MSRs for vcpu {vcpuid}", entries.len());
                let mut buf = Vec::with_capacity(entries.len() * 16);
                for entry in &entries {
                    buf.extend_from_slice(&entry.vfe_ident.to_le_bytes());
                    buf.extend_from_slice(&entry._pad.to_le_bytes());
                    buf.extend_from_slice(&entry.vfe_value.to_le_bytes());
                }
                buf
            }
            Err(e) => {
                eprintln!("[migrate] warning: MSR export vcpu {vcpuid}: {e:?}");
                Vec::new()
            }
        };

        // VMM_ARCH: pending interrupt state (NMI, EXTINT, exceptions)
        let vmm_arch = match hdl.data_op(bhyve_api::VDC_VMM_ARCH, 1)
            .for_vcpu(vcpuid)
            .read_all::<vdi_field_entry_v1>()
        {
            Ok(entries) => {
                let mut buf = Vec::with_capacity(entries.len() * 16);
                for entry in &entries {
                    buf.extend_from_slice(&entry.vfe_ident.to_le_bytes());
                    buf.extend_from_slice(&entry._pad.to_le_bytes());
                    buf.extend_from_slice(&entry.vfe_value.to_le_bytes());
                }
                eprintln!("[migrate] exported {} VMM_ARCH fields for vcpu {vcpuid}", entries.len());
                buf
            }
            Err(e) => {
                eprintln!("[migrate] VMM_ARCH export vcpu {vcpuid}: {e:?}");
                Vec::new()
            }
        };

        // FPU: skip for now (need VM_GET_FPU ioctl)
        let fpu = Vec::new();

        vcpus.push(VcpuStatePayload {
            vcpuid,
            registers,
            msrs,
            fpu,
            lapic,
            vmm_arch,
        });
    }

    // Export system device state (struct-based classes work with read_all)
    let mut devices = Vec::with_capacity(SYSTEM_DEVICES.len());
    for &(name, class, version) in SYSTEM_DEVICES {
        match read_class_all(hdl, class, version) {
            Ok(data) => {
                devices.push(DevicePayload {
                    name: name.to_string(),
                    class,
                    version,
                    data,
                });
            }
            Err(e) => {
                // Log but continue — some classes may not be available
                eprintln!("[migrate] warning: failed to export {name}: {e}");
            }
        }
    }

    Ok(DeviceState { vcpus, devices })
}

/// Import all kernel device state (per-vCPU + system devices).
///
/// Time data must already be imported. The VM must be paused.
pub fn import_device_state(
    hdl: &Arc<VmmHdl>,
    state: &DeviceState,
) -> Result<(), MigrateError> {
    // Import system device state FIRST (IOAPIC must be ready before LAPIC)
    for dev in &state.devices {
        if !dev.data.is_empty() {
            if let Err(e) = write_class_all(hdl, dev.class, dev.version, &dev.data) {
                eprintln!("[migrate] system device import {}: {e}", dev.name);
            }
        }
    }

    // Import per-vCPU state.
    //
    // Order: MSRs → LAPIC → Registers
    // MSRs before registers for GS_BASE/KERNEL_GS_BASE/STAR/LSTAR.
    // LAPIC before registers so timer state is correct.
    for vcpu_state in &state.vcpus {
        let vcpu = Vcpu::new_for_thread(
            vcpu_state.vcpuid,
            Arc::clone(hdl),
        );

        // 1. MSRs first
        if !vcpu_state.msrs.is_empty() {
            let entry_size = std::mem::size_of::<vdi_field_entry_v1>();
            if vcpu_state.msrs.len() % entry_size == 0 {
                if let Err(e) = write_class_vcpu(hdl, VDC_MSR, 1, vcpu_state.vcpuid, &vcpu_state.msrs) {
                    eprintln!("[migrate] MSR import vcpu {}: {e}", vcpu_state.vcpuid);
                }
            }
        }

        // 2. LAPIC
        if !vcpu_state.lapic.is_empty() {
            // The guest kernel may have masked the LAPIC timer when it
            // went idle (lvt_timer bit 16 = masked). If the timer is
            // masked, vlapic_fire_timer won't deliver the interrupt
            // even when the callout fires. Unmask it so the first
            // timer interrupt wakes the guest scheduler.
            let mut lapic_data = vcpu_state.lapic.clone();
            // Import LAPIC state verbatim — no timer hacks needed.
            // With correct VMM_TIME adjustment, the kernel's
            // vm_denormalize_hrtime() produces correct absolute times.
            // vlapic_resume() handles timer re-arming.
            eprintln!("[migrate] importing LAPIC for vcpu {}: {} bytes", vcpu_state.vcpuid, lapic_data.len());
            match write_class_vcpu(hdl, VDC_LAPIC, 1, vcpu_state.vcpuid, &lapic_data) {
                Ok(()) => eprintln!("[migrate] LAPIC import OK for vcpu {}", vcpu_state.vcpuid),
                Err(e) => eprintln!("[migrate] LAPIC import FAILED vcpu {}: {e}", vcpu_state.vcpuid),
            }
        } else {
            eprintln!("[migrate] WARNING: no LAPIC data for vcpu {}", vcpu_state.vcpuid);
        }

        // 3. Registers (includes segment descriptors)
        if !vcpu_state.registers.is_empty() {
            import_vcpu_regs(&vcpu, &vcpu_state.registers)?;
        }

        // 4. VMM_ARCH (pending interrupt state)
        if !vcpu_state.vmm_arch.is_empty() {
            let entry_size = std::mem::size_of::<vdi_field_entry_v1>();
            if vcpu_state.vmm_arch.len() % entry_size == 0 {
                if let Err(e) = write_class_vcpu(
                    hdl, bhyve_api::VDC_VMM_ARCH, 1, vcpu_state.vcpuid, &vcpu_state.vmm_arch
                ) {
                    eprintln!("[migrate] VMM_ARCH import vcpu {}: {e}", vcpu_state.vcpuid);
                }
            }
        }
    }

    Ok(())
}


// Suppress unused import warnings for the constants list -- they
// document which classes we *could* use even if VCPU_CLASSES is
// consumed only by the array literal above.
#[allow(unused_imports)]
use bhyve_api::VDC_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_devices_are_non_empty() {
        assert!(!SYSTEM_DEVICES.is_empty());
    }

    #[test]
    fn vcpu_classes_are_non_empty() {
        assert!(!_VCPU_CLASSES.is_empty());
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
}
