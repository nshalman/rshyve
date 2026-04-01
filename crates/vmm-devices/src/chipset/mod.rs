// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Chipset emulation.
//!
//! Provides the i440FX chipset which creates a minimal PCI topology
//! with a host bridge, ISA/LPC bridge, and PCI bus.

pub mod i440fx;

pub use i440fx::I440FxChipset;
