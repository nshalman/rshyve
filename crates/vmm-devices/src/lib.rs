// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device model traits and implementations.
//!
//! All emulated devices implement the [`Lifecycle`] trait which provides
//! hooks for VM state transitions and migration support.

pub mod acpi;
pub mod acpi_gpe;
pub mod acpi_pm;
pub mod aml;
pub mod audio;
pub mod bhyve;
pub mod blkdev;
pub mod chipset;
pub mod display;
pub mod fwcfg;
pub mod hotplug;
pub mod input;
pub mod lifecycle;
pub mod migrate;
pub mod overlay;
pub mod pci;
pub mod pvpanic;
pub mod quiesce;
pub mod smbios;
pub mod testdev;
pub mod uart;

/// Metrics live in their own crate. The alias keeps
/// `vmm_devices::metrics::*` resolving for the control socket and the
/// vCPU plumbing, which reach it by that path.
pub use vmm_metrics as metrics;

pub use input::{InputBroker, KeyboardSink, PointerSink};
pub use lifecycle::{
    DeviceMigrateState, FlushError, FlushIntent, IndicatedState, Indicator,
    Lifecycle,
};
pub use migrate::{
    MigrateCtx, MigrateMulti, MigrateSingle, MigrateStateError, Migrator,
    PayloadOffer, PayloadOffers, PayloadOutput, PayloadOutputs, Schema,
    SchemaId,
};
pub use quiesce::{wait_all_quiesced, QuiesceGate, QuiesceReport};

#[cfg(test)]
mod metrics_alias {
    // The control socket and the vCPU plumbing reach metrics through
    // `vmm_devices::metrics`. The alias keeps that path valid after the
    // types moved into their own crate, so no caller has to be edited.

    #[test]
    fn vcpu_metrics_resolves_through_the_alias() {
        let m = crate::metrics::VcpuMetrics::new();
        let json = m.to_json(3);
        assert!(json.contains(r#""vcpu":3"#), "{json}");
    }
}
