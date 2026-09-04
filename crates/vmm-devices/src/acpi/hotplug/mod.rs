// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hotplug AML, one submodule per resource kind.
//!
//! Each submodule owns both halves of its feature: the devices it puts
//! under `\_SB`, and the body of the `_Exx` method the guest runs when
//! its GPE bit is raised. Keeping the pair together means the AML and
//! the register file it drives cannot drift apart.

use crate::acpi::AcpiConfig;
use crate::aml::Aml;

pub(super) mod cpu;
pub(super) mod mem;
pub(super) mod pci;

/// Emit every hotplug controller and slot device under `\_SB`.
pub(super) fn emit_devices(sb: &mut Aml, cfg: &AcpiConfig) {
    pci::emit_devices(sb, cfg);
    cpu::emit_devices(sb, cfg);
    mem::emit_devices(sb, cfg);
}

/// Emit the `_Exx` handlers. The method number is the GPE bit number,
/// so these match [`crate::acpi_gpe::GpeBit`].
pub(super) fn emit_gpe_handlers(aml: &mut Aml, cfg: &AcpiConfig) {
    aml.scope("\\_GPE", |gpe| {
        gpe.method("_E01", 0, false, |m| pci::gpe_body(m, cfg));
        gpe.method("_E02", 0, false, |m| cpu::gpe_body(m, cfg));
        gpe.method("_E03", 0, false, |m| mem::gpe_body(m, cfg));
    });
}
