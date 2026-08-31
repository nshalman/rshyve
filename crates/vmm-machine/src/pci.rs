// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI device attachment seam.
//!
//! The machine layer must attach devices without naming one. Each binary
//! supplies its own catalog closure; this module supplies the plumbing
//! that closure needs and the loop that drives it.

use std::sync::Arc;

use slog::{error, Logger};

use vmm_core::hdl::VmmHdl;
use vmm_core::machine::Machine;
use vmm_core::mem::{PhysMap, SegidAlloc};
use vmm_core::mmio::MmioBus;
use vmm_core::pio::PioBus;
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::InputBroker;

pub type PciDeviceHandle = Arc<dyn vmm_devices::pci::PciDevice>;
pub type LifecycleHandle = Arc<dyn vmm_devices::Lifecycle>;

/// One device produced by a catalog closure.
///
/// All four shapes are legal. `(Some, None)` is passthru, which carries no
/// `Lifecycle`. `(None, None)` is a driver the chipset already created,
/// such as hostbridge or lpc.
pub type CreatedPciDevice = (Option<PciDeviceHandle>, Option<LifecycleHandle>);

/// A binary's device catalog. `FnMut` so the closure can hold state;
/// `dyn` so this module stays object-safe and is not monomorphized once
/// per binary.
pub type PciDeviceFactory<'a> =
    dyn FnMut(&str, &PciDeviceCtx<'_>) -> anyhow::Result<CreatedPciDevice> + 'a;

/// Machine-owned plumbing a device catalog needs.
///
/// Carries no CLI config. `num_vcpus`, `vm_name` and the
/// migration-destination flag are captured by each binary's closure, so
/// this layer stays ignorant of `-s` grammar.
pub struct PciDeviceCtx<'a> {
    pub chipset: &'a I440FxChipset,
    pub bus_pio: &'a Arc<PioBus>,
    pub bus_mmio: &'a Arc<MmioBus>,
    pub physmap: &'a Arc<PhysMap>,
    pub vmm_hdl: &'a Arc<VmmHdl>,
    pub segids: &'a SegidAlloc,
    pub input: &'a Arc<InputBroker>,
    pub log: &'a Logger,
}

impl<'a> PciDeviceCtx<'a> {
    pub fn new(
        machine: &'a Machine,
        chipset: &'a I440FxChipset,
        input: &'a Arc<InputBroker>,
        log: &'a Logger,
    ) -> Self {
        Self {
            chipset,
            bus_pio: machine.bus_pio(),
            bus_mmio: machine.bus_mmio(),
            physmap: machine.physmap(),
            vmm_hdl: machine.hdl(),
            segids: machine.segids(),
            input,
            log,
        }
    }
}

/// Handles collected from one pass over the device specs.
///
/// Deliberately carries no AHCI-CD flag: that is computed by re-parsing
/// the spec string for a driver name, which device-agnostic plumbing
/// cannot do.
#[derive(Default)]
pub struct PciAttachment {
    pub pci_devices: Vec<PciDeviceHandle>,
    pub lifecycle_devices: Vec<LifecycleHandle>,
}

fn push_created(created: CreatedPciDevice, out: &mut PciAttachment) {
    let (pci, lifecycle) = created;
    // A device with no PCI handle contributes nothing, even when it
    // offers a Lifecycle. Preserved from init_pci_devices.
    let Some(pci) = pci else {
        return;
    };
    out.pci_devices.push(pci);
    if let Some(lifecycle) = lifecycle {
        out.lifecycle_devices.push(lifecycle);
    }
}

/// The attach loop, with the context already bound into `create`.
///
/// Split out because a `PciDeviceCtx` needs a live VM, so this is the
/// only shape a test can reach.
fn attach_each(
    specs: &[String],
    log: &Logger,
    mut create: impl FnMut(&str) -> anyhow::Result<CreatedPciDevice>,
) -> anyhow::Result<PciAttachment> {
    let mut out = PciAttachment::default();
    for spec in specs {
        match create(spec) {
            Ok(created) => push_created(created, &mut out),
            Err(e) => {
                error!(log, "failed to create PCI device";
                    "spec" => spec,
                    "error" => format!("{:#}", e),
                );
                return Err(e);
            }
        }
    }
    Ok(out)
}

/// Run `factory` over already-bootindex-stripped `slot,driver[,config]`
/// specs, in order, and collect the handles.
pub fn attach_pci_devices(
    ctx: &PciDeviceCtx<'_>,
    specs: &[String],
    factory: &mut PciDeviceFactory<'_>,
) -> anyhow::Result<PciAttachment> {
    attach_each(specs, ctx.log, |spec| factory(spec, ctx))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use slog::Logger;

    use vmm_core::common::RWOp;
    use vmm_devices::pci::{BarN, PciDevice};
    use vmm_devices::Lifecycle;

    use super::{
        attach_each, push_created, CreatedPciDevice, LifecycleHandle,
        PciAttachment, PciDeviceHandle,
    };

    struct StubPci;

    impl PciDevice for StubPci {
        fn cfg_read(&self, _offset: u8, _len: u8) -> u32 {
            0
        }
        fn cfg_write(&self, _offset: u8, _len: u8, _val: u32) {}
        fn bar_rw(&self, _bar: BarN, _offset: usize, _rwo: RWOp<'_>) {}
    }

    struct StubLifecycle;

    impl Lifecycle for StubLifecycle {
        fn type_name(&self) -> &'static str {
            "stub"
        }
    }

    fn pci() -> PciDeviceHandle {
        Arc::new(StubPci)
    }

    fn lifecycle() -> LifecycleHandle {
        Arc::new(StubLifecycle)
    }

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn collect(created: CreatedPciDevice) -> (usize, usize) {
        let mut out = PciAttachment::default();
        push_created(created, &mut out);
        (out.pci_devices.len(), out.lifecycle_devices.len())
    }

    #[test]
    fn normal_device_lands_in_both_vectors() {
        assert_eq!(collect((Some(pci()), Some(lifecycle()))), (1, 1));
    }

    #[test]
    fn passthru_shape_lands_only_in_pci_devices() {
        // passthru carries no Lifecycle and is never quiesced, paused or
        // migrated. The factory contract must keep permitting this.
        assert_eq!(collect((Some(pci()), None)), (1, 0));
    }

    #[test]
    fn chipset_owned_driver_contributes_nothing() {
        // hostbridge and lpc: the chipset already created them.
        assert_eq!(collect((None, None)), (0, 0));
    }

    #[test]
    fn lifecycle_without_a_pci_handle_is_skipped() {
        // Behavior preserved from init_pci_devices, which matched
        // Ok((None, _)) and dropped the tuple.
        assert_eq!(collect((None, Some(lifecycle()))), (0, 0));
    }

    #[test]
    fn every_spec_reaches_the_factory_in_order() {
        // Slot order decides PCI enumeration order, so the loop must not
        // reorder or skip.
        let specs = vec![
            "0,hostbridge".to_string(),
            "4,virtio-blk,/disk".to_string(),
            "5,virtio-rnd".to_string(),
        ];
        let mut seen: Vec<String> = Vec::new();
        let attached = attach_each(&specs, &null_log(), |spec| {
            seen.push(spec.to_string());
            Ok((Some(pci()), Some(lifecycle())))
        })
        .expect("factory never fails here");
        assert_eq!(seen, specs);
        assert_eq!(attached.pci_devices.len(), 3);
        assert_eq!(attached.lifecycle_devices.len(), 3);
    }

    #[test]
    fn a_failed_device_stops_the_remaining_specs() {
        // A half-built machine must not run, so the first error aborts
        // the pass and no later spec is created.
        let specs = vec![
            "4,virtio-blk,/disk".to_string(),
            "5,nvme,/missing".to_string(),
            "6,virtio-rnd".to_string(),
        ];
        let mut seen: Vec<String> = Vec::new();
        let Err(err) = attach_each(&specs, &null_log(), |spec| {
            seen.push(spec.to_string());
            anyhow::ensure!(!spec.contains("nvme"), "failed to open disk");
            Ok((Some(pci()), None))
        }) else {
            panic!("the nvme spec must fail");
        };
        assert_eq!(err.to_string(), "failed to open disk");
        assert_eq!(seen, ["4,virtio-blk,/disk", "5,nvme,/missing"]);
    }
}
