// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device model traits and implementations.
//!
//! All emulated devices implement the [`Lifecycle`] trait which provides
//! hooks for VM state transitions and migration support.

pub mod acpi;
pub mod acpi_pm;
pub mod metrics;
pub mod aml;
pub mod bhyve;
pub mod chipset;
pub mod fbuf;
pub mod fwcfg;
pub mod lifecycle;
pub mod migrate;
pub mod nvme;
pub mod pci;
pub mod smbios;
pub mod uart;
pub mod virtio;
pub mod passthru;
pub mod pvpanic;
pub mod xhci;

pub use lifecycle::{IndicatedState, Indicator, Lifecycle};
pub use migrate::{
    MigrateCtx, MigrateMulti, MigrateSingle, MigrateStateError,
    Migrator, PayloadOffer, PayloadOffers, PayloadOutput, PayloadOutputs,
    Schema, SchemaId,
};
