// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shape and version checks for state that arrives from a source host.
//!
//! Nothing here talks to the kernel except [`read_class_layouts`], so
//! the checks can be tested without a live bhyve instance.

use bhyve_api::{
    vdi_field_entry_v1, vdi_version_entry_v1, vm_reg_name, VDC_LAPIC, VDC_MSR,
    VDC_PMU_AMD, VDC_VERSION,
};
use std::mem::size_of;
use vmm_core::hdl::VmmHdl;

use super::SYSTEM_DEVICES;
use crate::codec::{DeviceState, MigrateError};

/// One register write decoded from a vCPU register blob.
#[derive(Debug, Clone, Copy)]
pub(super) enum RegWrite {
    /// Plain register: identifier and value.
    Reg(vm_reg_name, u64),
    /// Segment descriptor: identifier, base, limit and access.
    Seg(vm_reg_name, u64, u32, u32),
}

/// Bytes in one plain register entry of a register blob.
const REG_ENTRY_LEN: usize = 12;
/// Bytes in one segment descriptor entry of a register blob.
const SEG_ENTRY_LEN: usize = 20;
/// Ends the plain register list and starts the descriptor list.
const SEG_MARKER: u32 = 0xFFFF_FFFF;

fn reg_from_id(vcpuid: i32, id: u32) -> Result<vm_reg_name, MigrateError> {
    match vm_reg_name::from_repr(id as i32) {
        Some(reg) if !matches!(reg, vm_reg_name::VM_REG_LAST) => Ok(reg),
        _ => Err(MigrateError::State(format!(
            "vcpu {vcpuid}: invalid register id {id}"
        ))),
    }
}

/// Decode a register blob made by `export_vcpu_regs`.
///
/// All of the blob must decode. A short or trailing fragment means the
/// payload does not match the format this build writes. Applying only
/// the part that decodes would resume the guest with a mix of source
/// and destination registers.
pub(super) fn parse_vcpu_regs(
    vcpuid: i32,
    data: &[u8],
) -> Result<Vec<RegWrite>, MigrateError> {
    let mut out = Vec::new();
    let mut pos = 0usize;

    while pos < data.len() {
        let left = data.len() - pos;
        if left < 4 {
            return Err(MigrateError::State(format!(
                "vcpu {vcpuid}: register blob has {left} trailing bytes"
            )));
        }
        let id =
            u32::from_le_bytes(data[pos..pos + 4].try_into().expect("4 bytes"));

        if id != SEG_MARKER {
            if left < REG_ENTRY_LEN {
                return Err(MigrateError::State(format!(
                    "vcpu {vcpuid}: register entry needs {REG_ENTRY_LEN} \
                     bytes, {left} left"
                )));
            }
            let value = u64::from_le_bytes(
                data[pos + 4..pos + 12].try_into().expect("8 bytes"),
            );
            out.push(RegWrite::Reg(reg_from_id(vcpuid, id)?, value));
            pos += REG_ENTRY_LEN;
            continue;
        }

        if left < 12 {
            return Err(MigrateError::State(format!(
                "vcpu {vcpuid}: register blob ends at the descriptor count"
            )));
        }
        let count = u64::from_le_bytes(
            data[pos + 4..pos + 12].try_into().expect("8 bytes"),
        );
        pos += 12;
        let want = usize::try_from(count)
            .ok()
            .and_then(|c| c.checked_mul(SEG_ENTRY_LEN))
            .ok_or_else(|| {
                MigrateError::State(format!(
                    "vcpu {vcpuid}: register blob claims {count} descriptors"
                ))
            })?;
        let left = data.len() - pos;
        if left != want {
            return Err(MigrateError::State(format!(
                "vcpu {vcpuid}: {count} segment descriptors need {want} \
                 bytes, {left} left"
            )));
        }
        for _ in 0..count {
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
            out.push(RegWrite::Seg(
                reg_from_id(vcpuid, seg_id)?,
                base,
                limit,
                access,
            ));
            pos += SEG_ENTRY_LEN;
        }
        break;
    }

    Ok(out)
}

/// Registers a v9 source did not send.
const V10_REGS: [vm_reg_name; 6] = [
    vm_reg_name::VM_REG_GUEST_DR0,
    vm_reg_name::VM_REG_GUEST_DR1,
    vm_reg_name::VM_REG_GUEST_DR2,
    vm_reg_name::VM_REG_GUEST_DR3,
    vm_reg_name::VM_REG_GUEST_DR6,
    vm_reg_name::VM_REG_GUEST_INTR_SHADOW,
];

/// Every register this build exports, each once, and nothing else. A
/// register left out would resume at the destination's reset value.
///
/// A v9 source sends the list without [`V10_REGS`].
fn check_register_set(
    vcpuid: i32,
    writes: &[RegWrite],
    current_peer: bool,
) -> Result<(), MigrateError> {
    use std::collections::BTreeSet;
    let id = |r: vm_reg_name| r as i32;
    let mut want: BTreeSet<i32> =
        super::VCPU_REGS.iter().map(|&r| id(r)).collect();
    want.insert(id(vm_reg_name::VM_REG_GUEST_INTR_SHADOW));
    if !current_peer {
        for r in V10_REGS {
            want.remove(&id(r));
        }
    }
    let want_seg: BTreeSet<i32> =
        super::VCPU_SEG_DESCS.iter().map(|&r| id(r)).collect();

    let (mut regs, mut segs) = (BTreeSet::new(), BTreeSet::new());
    for write in writes {
        let (set, reg) = match *write {
            RegWrite::Reg(reg, value) => {
                // get_reg answers 0 or 1 on both VMX and SVM.
                if matches!(reg, vm_reg_name::VM_REG_GUEST_INTR_SHADOW)
                    && value > 1
                {
                    return Err(MigrateError::State(format!(
                        "vcpu {vcpuid}: interrupt shadow {value:#x}"
                    )));
                }
                (&mut regs, reg)
            }
            RegWrite::Seg(reg, ..) => (&mut segs, reg),
        };
        if !set.insert(id(reg)) {
            return Err(MigrateError::State(format!(
                "vcpu {vcpuid}: register {reg:?} appears twice"
            )));
        }
    }
    if regs != want || segs != want_seg {
        let missing: Vec<i32> = want
            .difference(&regs)
            .chain(want_seg.difference(&segs))
            .copied()
            .collect();
        let extra: Vec<i32> = regs
            .difference(&want)
            .chain(segs.difference(&want_seg))
            .copied()
            .collect();
        return Err(MigrateError::State(format!(
            "vcpu {vcpuid}: register set differs: missing ids {missing:?}, \
             unexpected ids {extra:?}"
        )));
    }
    Ok(())
}

/// The vmm-data class shapes the local kernel registers, read from
/// VDC_VERSION: one entry per (class, version) pair, with the total
/// length the kernel expects (`vve_len_expect`, 0 when variable) and
/// the length of one list element (`vve_len_per_item`, 0 when the class
/// is not a list).
pub(super) fn read_class_layouts(
    hdl: &VmmHdl,
) -> Result<Vec<vdi_version_entry_v1>, MigrateError> {
    super::with_vm_data(
        hdl,
        VDC_VERSION,
        1,
        "read local class versions",
        |op| op.read_all::<vdi_version_entry_v1>(),
    )
    .map_err(|e| MigrateError::State(e.to_string()))
}

/// Check one state blob against the local kernel's shape for its class.
///
/// Checked: the kernel has this exact class and version, the total
/// length for a fixed-size class, and the element length for a list
/// class. Not checked: the field layout inside a struct of the same
/// class and version, which the kernel does not describe. The version
/// number is the kernel's own contract for that layout.
pub(super) fn check_class_blob(
    layouts: &[vdi_version_entry_v1],
    what: &str,
    class: u16,
    version: u16,
    len: usize,
) -> Result<(), MigrateError> {
    if len == 0 {
        return Err(MigrateError::State(format!(
            "{what}: class {class} state is missing from the payload"
        )));
    }
    let Some(entry) = layouts
        .iter()
        .find(|e| e.vve_class == class && e.vve_version == version)
    else {
        let have: Vec<u16> = layouts
            .iter()
            .filter(|e| e.vve_class == class)
            .map(|e| e.vve_version)
            .collect();
        return Err(MigrateError::State(format!(
            "{what}: destination kernel does not have class {class} version \
             {version}, it has versions {have:?}"
        )));
    };

    if entry.vve_len_per_item != 0 {
        let item = entry.vve_len_per_item as usize;
        if !len.is_multiple_of(item) {
            return Err(MigrateError::State(format!(
                "{what}: {len} bytes is not a whole number of the \
                 destination kernel's {item}-byte entries for class {class}"
            )));
        }
    } else if entry.vve_len_expect != 0 && len != entry.vve_len_expect as usize
    {
        return Err(MigrateError::State(format!(
            "{what}: {len} bytes, destination kernel expects {} for class \
             {class} version {version}",
            entry.vve_len_expect
        )));
    }
    Ok(())
}

/// Check a blob of `vdi_field_entry_v1` records (MSR, VMM_ARCH).
///
/// This build serializes the records itself, so the kernel's element
/// size must match the struct that was written.
fn check_field_entry_blob(
    layouts: &[vdi_version_entry_v1],
    what: &str,
    class: u16,
    version: u16,
    len: usize,
) -> Result<(), MigrateError> {
    check_class_blob(layouts, what, class, version, len)?;
    let item = layouts
        .iter()
        .find(|e| e.vve_class == class && e.vve_version == version)
        .map_or(0, |e| e.vve_len_per_item as usize);
    let local = size_of::<vdi_field_entry_v1>();
    if item != local {
        return Err(MigrateError::State(format!(
            "{what}: destination kernel uses {item}-byte entries for class \
             {class}, this build writes {local}-byte entries"
        )));
    }
    Ok(())
}

/// Check a whole payload before any of it is applied.
///
/// Import is all or nothing. A guest that resumes with part of its CPU
/// state is worse than a migration that fails, because a failed
/// migration leaves the source running.
///
/// Checked: one state per vCPU with a usable id, every class the
/// destination kernel needs at a class and version that kernel has,
/// each blob the length that kernel expects, a register blob that
/// decodes whole, and every system device class present exactly once.
/// Not checked: the values inside the blobs, which only the kernel can
/// judge. The XSAVE areas are checked by [`crate::xsave::XsavePlan`].
pub(super) fn check_device_state(
    state: &DeviceState,
    layouts: &[vdi_version_entry_v1],
    num_cpus: u32,
    amd_host: bool,
    current_peer: bool,
) -> Result<(), MigrateError> {
    // One payload per vCPU, no more and no fewer. The kernel accepts
    // any id below VM_MAXCPU, so a payload for a vCPU this VM never
    // activated writes MSRs, LAPIC and run state into one that has no
    // thread; a missing one leaves that AP at reset and the guest
    // kernel hangs waiting for it.
    if state.vcpus.len() != num_cpus as usize {
        return Err(MigrateError::State(format!(
            "payload has {} vCPU states, this VM has {num_cpus} vCPUs",
            state.vcpus.len(),
        )));
    }

    let mut seen_vcpu = std::collections::HashSet::new();
    for vcpu in &state.vcpus {
        let id = vcpu.vcpuid;
        if id < 0 || id as u32 >= num_cpus {
            return Err(MigrateError::State(format!(
                "payload has vcpu id {id}, outside 0..{num_cpus}"
            )));
        }
        if !seen_vcpu.insert(id) {
            return Err(MigrateError::State(format!(
                "payload has two states for vcpu {id}"
            )));
        }
        check_field_entry_blob(
            layouts,
            &format!("vcpu {id} MSR"),
            VDC_MSR,
            1,
            vcpu.msrs.len(),
        )?;
        check_field_entry_blob(
            layouts,
            &format!("vcpu {id} VMM_ARCH"),
            bhyve_api::VDC_VMM_ARCH,
            1,
            vcpu.vmm_arch.len(),
        )?;
        check_class_blob(
            layouts,
            &format!("vcpu {id} LAPIC"),
            VDC_LAPIC,
            1,
            vcpu.lapic.len(),
        )?;
        if let Some(pmu) = &vcpu.pmu_amd {
            // Only SVM keeps the state behind this class, though every
            // v18 kernel lists it.
            if !amd_host {
                return Err(MigrateError::State(format!(
                    "vcpu {id}: payload carries AMD PMU state, and this \
                     host is not AMD"
                )));
            }
            check_class_blob(
                layouts,
                &format!("vcpu {id} PMU_AMD"),
                VDC_PMU_AMD,
                1,
                pmu.len(),
            )?;
        }
        if vcpu.registers.is_empty() {
            return Err(MigrateError::State(format!(
                "vcpu {id}: register state is missing from the payload"
            )));
        }
        let writes = parse_vcpu_regs(id, &vcpu.registers)?;
        check_register_set(id, &writes, current_peer)?;
    }

    // A source carries it for every vCPU or for none.
    let with_pmu = state.vcpus.iter().filter(|v| v.pmu_amd.is_some()).count();
    if with_pmu != 0 && with_pmu != state.vcpus.len() {
        return Err(MigrateError::State(format!(
            "payload carries AMD PMU state for {with_pmu} of {} vCPUs",
            state.vcpus.len()
        )));
    }

    let mut seen_class = std::collections::HashSet::new();
    for dev in &state.devices {
        // The label comes from the local table, never from the payload:
        // a peer must not choose what goes in our log. An unknown class
        // is refused because nothing here would ever apply it.
        let Some(&(label, _, _)) =
            SYSTEM_DEVICES.iter().find(|&&(_, c, _)| c == dev.class)
        else {
            return Err(MigrateError::State(format!(
                "payload has an unknown device class {}",
                dev.class
            )));
        };
        if !seen_class.insert(dev.class) {
            return Err(MigrateError::State(format!(
                "{label}: payload has two states for class {}",
                dev.class
            )));
        }
        check_class_blob(
            layouts,
            label,
            dev.class,
            dev.version,
            dev.data.len(),
        )?;
    }
    for &(name, class, _) in SYSTEM_DEVICES {
        if !seen_class.contains(&class) {
            return Err(MigrateError::State(format!(
                "{name}: class {class} state is missing from the payload"
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use bhyve_api::VDC_VMM_TIME;

    use crate::codec::{DevicePayload, VcpuStatePayload};

    pub(crate) fn entry(
        class: u16,
        version: u16,
        len_expect: u16,
        len_per_item: u16,
    ) -> vdi_version_entry_v1 {
        vdi_version_entry_v1 {
            vve_class: class,
            vve_version: version,
            vve_len_expect: len_expect,
            vve_len_per_item: len_per_item,
        }
    }

    pub(crate) const TEST_DEV_LEN: usize = 8;

    pub(crate) const PMU_AMD_LEN: usize =
        size_of::<bhyve_api::vdi_pmu_amd_v1>();

    /// An FXSAVE-only host: the legacy region and the XSAVE header.
    pub(crate) const TEST_XSAVE_LEN: usize = 576;

    pub(crate) fn test_xsave() -> crate::xsave::XsaveLayout {
        use crate::xsave::{XsaveComponent, XsaveLayout};
        XsaveLayout {
            size: TEST_XSAVE_LEN as u32,
            components: [1, 2]
                .into_iter()
                .map(|feature| XsaveComponent {
                    feature,
                    size: 512,
                    offset: 0,
                })
                .collect(),
        }
    }

    /// The class table a matching destination kernel reports.
    pub(crate) fn kernel_layouts() -> Vec<vdi_version_entry_v1> {
        let item = size_of::<vdi_field_entry_v1>() as u16;
        let lapic_len = size_of::<bhyve_api::vdi_lapic_v1>() as u16;
        let mut out = vec![
            entry(VDC_MSR, 1, 0, item),
            entry(bhyve_api::VDC_VMM_ARCH, 1, 0, item),
            entry(VDC_LAPIC, 1, lapic_len, 0),
            entry(VDC_VMM_TIME, 1, 0, 0),
            entry(VDC_PMU_AMD, 1, PMU_AMD_LEN as u16, 0),
        ];
        for &(_, class, version) in SYSTEM_DEVICES {
            out.push(entry(class, version, TEST_DEV_LEN as u16, 0));
        }
        out
    }

    pub(crate) fn field_entries(count: usize) -> Vec<u8> {
        vec![0u8; count * size_of::<vdi_field_entry_v1>()]
    }

    /// Build a register blob in the same format as `export_vcpu_regs`.
    pub(crate) fn reg_blob(
        regs: &[(vm_reg_name, u64)],
        segs: &[vm_reg_name],
    ) -> Vec<u8> {
        let mut data = Vec::new();
        for &(reg, val) in regs {
            data.extend_from_slice(&(reg as u32).to_le_bytes());
            data.extend_from_slice(&val.to_le_bytes());
        }
        data.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        data.extend_from_slice(&(segs.len() as u64).to_le_bytes());
        for &reg in segs {
            data.extend_from_slice(&(reg as u32).to_le_bytes());
            data.extend_from_slice(&0u64.to_le_bytes());
            data.extend_from_slice(&0u32.to_le_bytes());
            data.extend_from_slice(&0u32.to_le_bytes());
        }
        data
    }

    /// Every register a current source sends.
    pub(crate) fn full_reg_blob(shadow: u64) -> Vec<u8> {
        let mut regs: Vec<(vm_reg_name, u64)> = super::super::VCPU_REGS
            .iter()
            .map(|&r| (r, 0x1234))
            .collect();
        regs.push((vm_reg_name::VM_REG_GUEST_INTR_SHADOW, shadow));
        reg_blob(&regs, super::super::VCPU_SEG_DESCS)
    }

    pub(crate) fn default_reg_blob() -> Vec<u8> {
        full_reg_blob(0)
    }

    /// The number of writes [`default_reg_blob`] decodes to.
    pub(crate) fn full_reg_count() -> usize {
        super::super::VCPU_REGS.len() + 1 + super::super::VCPU_SEG_DESCS.len()
    }

    pub(crate) fn vcpu_payload(vcpuid: i32) -> VcpuStatePayload {
        VcpuStatePayload {
            vcpuid,
            registers: default_reg_blob(),
            msrs: field_entries(4),
            fpu: vec![0u8; TEST_XSAVE_LEN],
            lapic: vec![0u8; size_of::<bhyve_api::vdi_lapic_v1>()],
            vmm_arch: field_entries(2),
            run_state: bhyve_api::VRS_RUN,
            sipi_vector: 0,
            pmu_amd: None,
        }
    }

    pub(crate) fn device_payloads() -> Vec<DevicePayload> {
        SYSTEM_DEVICES
            .iter()
            .map(|&(name, class, version)| DevicePayload {
                name: name.to_string(),
                class,
                version,
                data: vec![0u8; TEST_DEV_LEN],
            })
            .collect()
    }

    pub(crate) fn good_state() -> DeviceState {
        DeviceState {
            vcpus: vec![vcpu_payload(0)],
            devices: device_payloads(),
            emulated: Vec::new(),
            hyperv: None,
            platform: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn class_blob_needs_the_local_kernel_to_have_the_class() {
        let layouts = kernel_layouts();
        let err = check_class_blob(&layouts, "made up", 0xFFFF, 1, 8)
            .expect_err("unknown class must fail");
        assert!(err.to_string().contains("65535"), "{err}");
    }

    #[test]
    fn register_blob_rejects_trailing_bytes() {
        let mut blob = default_reg_blob();
        blob.push(0);
        assert!(parse_vcpu_regs(0, &blob).is_err());
    }

    fn decoded(blob: &[u8]) -> Vec<RegWrite> {
        parse_vcpu_regs(0, blob).expect("decodes")
    }

    #[test]
    fn the_register_set_must_be_whole() {
        let full = decoded(&default_reg_blob());
        check_register_set(0, &full, true).expect("current");
        check_register_set(0, &full, false).expect_err("v9 sends no DRs");

        let mut short = full.clone();
        short.retain(|w| {
            !matches!(w, RegWrite::Reg(vm_reg_name::VM_REG_GUEST_DR6, _))
        });
        let error = check_register_set(0, &short, true).expect_err("no DR6");
        assert!(error.to_string().contains("missing"), "{error}");

        let mut twice = full.clone();
        twice.push(full[0]);
        let error = check_register_set(0, &twice, true).expect_err("twice");
        assert!(error.to_string().contains("twice"), "{error}");

        let v9: Vec<RegWrite> = full
            .iter()
            .copied()
            .filter(|w| {
                !matches!(w, RegWrite::Reg(r, _)
                    if V10_REGS.iter().any(|&n| n as i32 == *r as i32))
            })
            .collect();
        check_register_set(0, &v9, false).expect("a v9 source");
        check_register_set(0, &v9, true).expect_err("a v10 source sends all");

        let bad = decoded(&full_reg_blob(7));
        let error = check_register_set(0, &bad, true).expect_err("shadow 7");
        assert!(error.to_string().contains("shadow"), "{error}");
    }

    #[test]
    fn reg_blob_round_trips() {
        let blob = reg_blob(
            &[
                (vm_reg_name::VM_REG_GUEST_RAX, 1),
                (vm_reg_name::VM_REG_GUEST_RBX, 2),
            ],
            &[vm_reg_name::VM_REG_GUEST_CS, vm_reg_name::VM_REG_GUEST_GDTR],
        );
        let writes = parse_vcpu_regs(0, &blob).expect("blob must decode");
        assert_eq!(writes.len(), 4);
        assert!(matches!(
            writes[0],
            RegWrite::Reg(vm_reg_name::VM_REG_GUEST_RAX, 1)
        ));
        assert!(matches!(
            writes[1],
            RegWrite::Reg(vm_reg_name::VM_REG_GUEST_RBX, 2)
        ));
        assert!(matches!(
            writes[2],
            RegWrite::Seg(vm_reg_name::VM_REG_GUEST_CS, 0, 0, 0)
        ));
        assert!(matches!(
            writes[3],
            RegWrite::Seg(vm_reg_name::VM_REG_GUEST_GDTR, 0, 0, 0)
        ));
    }
}
