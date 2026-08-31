// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! vCPU activation and thread spawn.

use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{anyhow, Context};
use slog::{info, warn, Logger};
use vmm_core::machine::Machine;
use vmm_core::msr::MsrHandler;
use vmm_core::vcpu::Vcpu;
use vmm_devices::metrics::VcpuMetrics;

use crate::vcpu_tasks::{self, VcpuEvent};

/// The boot-strap processor. Every other vCPU is an application
/// processor and waits for a SIPI from this one.
const BSP_ID: i32 = 0;

/// How the boot-strap processor is entered.
pub enum BspEntry<'a> {
    /// Firmware reset vector, that is UEFI. Resets the BSP only.
    Firmware,
    /// The caller programs the BSP itself: direct boot, PVH.
    Custom(&'a dyn Fn(&Vcpu) -> anyhow::Result<()>),
}

/// Activate every vCPU, then program the BSP for the chosen entry.
pub fn activate_vcpus(
    machine: &Machine,
    vmexit_on_hlt: bool,
    bsp: BspEntry<'_>,
    log: &Logger,
) -> anyhow::Result<()> {
    activate_all_vcpus(machine, vmexit_on_hlt)?;

    match bsp {
        BspEntry::Firmware => activate_uefi_bsp_only(machine, log)?,
        BspEntry::Custom(setup) => {
            if let Some(bsp) = machine.vcpus().first() {
                setup(bsp).context("failed to set up BSP")?;
            }
            info!(log, "BSP configured by caller");
        }
    }

    Ok(())
}

/// Activate every vCPU and set the per-vCPU capabilities.
///
/// This touches no reset state and no run state. Keeping activation
/// apart from reset is what makes the AP asymmetry below auditable.
pub fn activate_all_vcpus(
    machine: &Machine,
    vmexit_on_hlt: bool,
) -> anyhow::Result<()> {
    for vcpu in machine.vcpus() {
        vcpu.activate().with_context(|| {
            format!("failed to activate vCPU {}", vcpu.id())
        })?;

        if vmexit_on_hlt {
            machine
                .hdl()
                .set_capability(
                    vcpu.id(),
                    bhyve_api::vm_cap_type::VM_CAP_HALT_EXIT as i32,
                    1,
                )
                .with_context(|| {
                    format!("failed to set HALT_EXIT on vCPU {}", vcpu.id())
                })?;
        }
    }
    Ok(())
}

/// Which vCPUs the UEFI boot path resets.
///
/// Only the BSP, whatever the CPU count. `Vcpu::setup_bsp` calls
/// `reboot_state()`, and that on an AP clears the state the guest's
/// INIT/SIPI bring-up depends on, which costs hundreds of seconds of
/// SMP boot time. Do not widen this list.
pub fn uefi_reset_targets(num_cpus: u32) -> Vec<i32> {
    // A vCPU id is an i32 in the bhyve API. Ids ascend, so stopping at
    // the first one that does not fit keeps the list total.
    (0..num_cpus)
        .map_while(|id| i32::try_from(id).ok())
        .filter(|id| *id == BSP_ID)
        .collect()
}

/// Reset the BSP for firmware entry, and only the BSP.
pub fn activate_uefi_bsp_only(
    machine: &Machine,
    log: &Logger,
) -> anyhow::Result<()> {
    for id in uefi_reset_targets(machine.num_cpus()) {
        let index = usize::try_from(id)
            .map_err(|_| anyhow!("reset target vCPU {id} is negative"))?;
        let vcpu = machine
            .vcpus()
            .get(index)
            .ok_or_else(|| anyhow!("reset target vCPU {id} does not exist"))?;
        vcpu.setup_bsp()
            .with_context(|| format!("failed to set up BSP vCPU {id}"))?;
    }

    check_aps_awaiting_sipi(machine, log);
    Ok(())
}

/// Post-condition for [`activate_uefi_bsp_only`]: every AP still waits
/// for SIPI, that is `VRS_RUN` is clear.
///
/// This warns and does not fail. No bhyve API version has been checked
/// for how it reports AP run state at this point, so a hard error could
/// break a UEFI boot that works today. Make it an error once a live
/// host confirms the state.
pub fn check_aps_awaiting_sipi(machine: &Machine, log: &Logger) {
    for vcpu in machine.vcpus() {
        if vcpu.id() == BSP_ID {
            continue;
        }
        match vcpu.get_run_state() {
            Ok((state, _sipi_vector)) if state & bhyve_api::VRS_RUN != 0 => {
                warn!(log, "AP is runnable before SIPI";
                    "vcpu" => vcpu.id(),
                    "run_state" => format!("{state:#x}"),
                );
            }
            Ok(_) => {}
            Err(error) => {
                warn!(log, "failed to read AP run state";
                    "vcpu" => vcpu.id(),
                    "error" => %error,
                );
            }
        }
    }
}

/// Spawn per-vCPU threads and return the event channel, the join
/// handles, and the per-vCPU metrics.
///
/// `msr_handler` is a trait object so this crate does not name the
/// crate that supplies the synthetic MSRs. Only rshyve has one.
pub fn spawn_vcpus(
    machine: &Machine,
    num_cpus: u32,
    api_version: u32,
    msr_handler: Option<Arc<dyn MsrHandler>>,
    log: &Logger,
) -> (
    mpsc::Receiver<VcpuEvent>,
    Vec<JoinHandle<()>>,
    Vec<Arc<VcpuMetrics>>,
) {
    info!(log, "starting vCPU threads");

    let vcpu_metrics: Vec<Arc<VcpuMetrics>> = (0..num_cpus)
        .map(|_| Arc::new(VcpuMetrics::new()))
        .collect();

    let (event_rx, handles) = vcpu_tasks::spawn_vcpu_threads(
        machine.vcpus(),
        machine.bus_pio(),
        machine.bus_mmio(),
        machine.hdl().clone(),
        api_version,
        &vcpu_metrics,
        msr_handler,
        log,
    );

    (event_rx, handles, vcpu_metrics)
}

#[cfg(test)]
mod tests {
    use super::uefi_reset_targets;

    /// The activation paths need a live bhyve VM, and a unit test has
    /// none. Read the invariant out of the source instead. The slice
    /// stops at the test module so a test literal cannot satisfy a
    /// check that the implementation no longer does.
    fn implementation() -> &'static str {
        let src = include_str!("vcpu.rs");
        let end = src.find("#[cfg(test)]").expect("test module attribute");
        &src[..end]
    }

    #[test]
    fn uefi_reset_targets_selects_the_bsp_and_no_ap() {
        // Resetting an AP clears the state the guest's INIT/SIPI
        // bring-up depends on, which costs hundreds of seconds of SMP
        // boot time.
        for num_cpus in [1u32, 2, 8, 64] {
            let targets = uefi_reset_targets(num_cpus);
            assert_eq!(targets, vec![0], "only the BSP is reset");

            let untouched: Vec<i32> = (0..num_cpus as i32)
                .filter(|id| !targets.contains(id))
                .collect();
            assert_eq!(
                untouched,
                (1..num_cpus as i32).collect::<Vec<i32>>(),
                "every AP stays in INIT, waiting for SIPI",
            );
        }
    }

    #[test]
    fn uefi_reset_targets_derives_from_the_cpu_count() {
        // A target list that ignores its argument can name a vCPU that
        // does not exist, and it cannot describe the AP complement.
        assert!(uefi_reset_targets(0).is_empty());
    }

    #[test]
    fn uefi_bsp_reset_goes_through_the_target_list() {
        let src = implementation();
        let start = src
            .find("pub fn activate_uefi_bsp_only")
            .expect("activate_uefi_bsp_only");
        let body = &src[start..];
        let end = body.find("\n}\n").expect("end of activate_uefi_bsp_only");
        let body = &body[..end];

        assert!(
            body.contains("uefi_reset_targets(machine.num_cpus())"),
            "the target list is the only place the AP asymmetry is decided",
        );
        assert!(
            !body.contains("machine.vcpus()"),
            "iterating every vCPU here resets the APs",
        );
    }
}
