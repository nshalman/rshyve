// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device model traits and implementations.
//!
//! All emulated devices implement the [`Lifecycle`] trait which provides
//! hooks for VM state transitions and migration support.

pub mod bhyve;
pub mod chipset;
pub mod lifecycle;
pub mod migrate;
pub mod pci;
pub mod uart;

pub use lifecycle::{IndicatedState, Indicator, Lifecycle};
pub use migrate::{
    MigrateCtx, MigrateMulti, MigrateSingle, MigrateStateError,
    Migrator, PayloadOffer, PayloadOffers, PayloadOutput, PayloadOutputs,
    Schema, SchemaId,
};
