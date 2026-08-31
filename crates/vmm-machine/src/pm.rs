// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI PM timer, PM1 registers, ISA IRQ trigger modes, and pvpanic.

use std::sync::Arc;

use anyhow::Context;
use slog::{info, warn, Logger};
use vmm_core::common::RWOp;
use vmm_core::machine::Machine;
use vmm_core::pio::PioFn;
use vmm_devices::acpi_pm::{AcpiPm, HdlSuspendSink, SuspendSink};
use vmm_devices::bhyve::pmtimer::{BhyvePmTimer, PMBASE_DEFAULT};
use vmm_devices::pvpanic::{PvPanic, PVPANIC_IOPORT};

/// Timer and power-management device handles kept alive for the VM's
/// lifetime.
pub struct TimerAndPmDevices {
    pub pmtimer: Arc<BhyvePmTimer>,
    pub acpi_pm: Arc<AcpiPm>,
    pub pvpanic: Arc<PvPanic>,
    pub suspend_sink: Arc<dyn SuspendSink>,
}

/// Configure the ACPI PM timer, ISA IRQ trigger modes for SCI and PCI,
/// ACPI PM1 registers, and the pvpanic PIO device.
pub fn setup_timers_and_pm(
    machine: &Machine,
    log: &Logger,
) -> anyhow::Result<TimerAndPmDevices> {
    // Configure ACPI PM timer in kernel
    let pmtimer = BhyvePmTimer::create(machine.hdl().clone(), PMBASE_DEFAULT);
    pmtimer.attach().context("failed to configure PM timer")?;

    // Set SCI IRQ (9) to level-triggered mode in the kernel ATPIC.
    machine
        .hdl()
        .isa_set_irq_trigger(9, true)
        .context("failed to set SCI IRQ trigger mode")?;

    // Set PCI IRQs 10 and 11 to level-triggered (firmware assigns these
    // via PIR registers to PIRQ links A-D).
    machine
        .hdl()
        .isa_set_irq_trigger(10, true)
        .context("failed to set IRQ 10 trigger mode")?;
    machine
        .hdl()
        .isa_set_irq_trigger(11, true)
        .context("failed to set IRQ 11 trigger mode")?;
    info!(log, "PM timer attached"; "port" => format!("{:#x}", pmtimer.port()));

    // Create ACPI PM1 event/enable/control registers + reset handler
    let suspend_sink: Arc<dyn SuspendSink> = HdlSuspendSink::new(
        machine.hdl().clone(),
        log.new(slog::o!("component" => "acpi-pm")),
    );
    let acpi_pm = AcpiPm::create_and_attach(
        machine.bus_pio(),
        Arc::clone(&suspend_sink),
        log.new(slog::o!("component" => "acpi-pm")),
    )
    .context("failed to attach ACPI PM registers")?;
    info!(log, "ACPI PM registers attached"; "pmbase" => format!("{:#x}", 0x400));

    // pvpanic device: the guest writes to port 0x505 on kernel panic
    let pvpanic = PvPanic::new(log.clone());
    {
        let pvpanic = Arc::clone(&pvpanic);
        let handler: Arc<PioFn> = Arc::new(move |_offset, rwo| match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(pvpanic.pio_read());
            }
            RWOp::Write(wo) => {
                pvpanic.pio_write(wo.read_u8());
            }
        });
        if let Err(e) = machine.bus_pio().register(PVPANIC_IOPORT, 1, handler) {
            warn!(log, "failed to register pvpanic PIO handler"; "error" => %e);
        }
    }

    Ok(TimerAndPmDevices {
        pmtimer,
        acpi_pm,
        pvpanic,
        suspend_sink,
    })
}
