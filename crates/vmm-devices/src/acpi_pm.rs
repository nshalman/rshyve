// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI Power Management register emulation.
//!
//! Implements the PM1 event, enable, and control registers that the
//! UEFI firmware and OS poll for power management events. Also handles
//! the reset register at 0xCF9.
//!
//! Without these handlers, the firmware's boot timeout never fires
//! because it polls PM1_STS for timer overflow events.

use std::sync::{Arc, Mutex};

use slog;
use vmm_core::common::RWOp;
use vmm_core::pio::{PioBus, PioFn};

/// ACPI PM base I/O port.
const PMBASE: u16 = 0x400;

/// SMI Command port for ACPI enable/disable.
const SMI_CMD_PORT: u16 = 0xB2;
/// Value written to SMI_CMD to enable ACPI (set SCI_EN).
const ACPI_ENABLE: u8 = 0xA0;
/// Value written to SMI_CMD to disable ACPI (clear SCI_EN).
const ACPI_DISABLE: u8 = 0xA1;

/// PM1 Status register bit definitions.
const PM1_TMR_STS: u16 = 0x0001;
const PM1_BM_STS: u16 = 0x0010;
const PM1_GBL_STS: u16 = 0x0020;
const PM1_PWRBTN_STS: u16 = 0x0100;
const PM1_SLPBTN_STS: u16 = 0x0200;
const PM1_RTC_STS: u16 = 0x0400;
const PM1_WAK_STS: u16 = 0x8000;

/// Clearable status bits (write-1-to-clear).
const PM1_STS_CLEARABLE: u16 =
    PM1_WAK_STS | PM1_RTC_STS | PM1_SLPBTN_STS | PM1_PWRBTN_STS | PM1_BM_STS;

/// PM1 Enable register bit definitions.
const PM1_TMR_EN: u16 = 0x0001;
const PM1_GBL_EN: u16 = 0x0020;
const PM1_PWRBTN_EN: u16 = 0x0100;
const PM1_SLPBTN_EN: u16 = 0x0200;
const PM1_RTC_EN: u16 = 0x0400;

/// Writable enable bits.
const PM1_EN_WRITABLE: u16 =
    PM1_RTC_EN | PM1_PWRBTN_EN | PM1_GBL_EN | PM1_TMR_EN;

/// PM1 Control register bit definitions.
const PM1_SCI_EN: u16 = 0x0001;
const PM1_SLP_TYP_MASK: u16 = 0x1C00;
const PM1_SLP_EN: u16 = 0x2000;
const PM1_ALWAYS_ZERO: u16 = 0xC003;

/// ACPI PM1 register state.
pub struct AcpiPm {
    log: slog::Logger,
    inner: Mutex<AcpiPmInner>,
}

struct AcpiPmInner {
    pm1_status: u16,
    pm1_enable: u16,
    pm1_control: u16,
}

impl AcpiPm {
    /// Create and attach the ACPI PM registers to the PIO bus.
    ///
    /// Registers handlers for:
    /// - PM1 Status at PMBASE (0xB000, 2 bytes)
    /// - PM1 Enable at PMBASE+2 (0xB002, 2 bytes)
    /// - PM1 Control at PMBASE+4 (0xB004, 2 bytes)
    /// - Reset register at 0xCF9 (1 byte)
    pub fn create_and_attach(bus_pio: &PioBus, log: slog::Logger) -> Arc<Self> {
        let pm = Arc::new(Self {
            log,
            inner: Mutex::new(AcpiPmInner {
                pm1_status: 0,
                pm1_enable: 0,
                // SCI_EN starts clear — OS sets it via SMI_CMD write
                pm1_control: 0,
            }),
        });

        // PM1 Event block: status (2B) + enable (2B) = 4 bytes at PMBASE
        let pm_evt = Arc::clone(&pm);
        let evt_handler: Arc<PioFn> =
            Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                pm_evt.handle_pm1_evt(offset, rwo);
            });
        if let Err(e) = bus_pio.register(PMBASE, 4, evt_handler) {
            slog::warn!(pm.log, "failed to register PM1 event handler"; "error" => %e);
        }

        // PM1 Control block: 2 bytes at PMBASE+4
        let pm_cnt = Arc::clone(&pm);
        let cnt_handler: Arc<PioFn> =
            Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                pm_cnt.handle_pm1_cnt(offset, rwo);
            });
        if let Err(e) = bus_pio.register(PMBASE + 4, 2, cnt_handler) {
            slog::warn!(pm.log, "failed to register PM1 control handler"; "error" => %e);
        }

        // Reset register at 0xCF9
        let pm_rst = Arc::clone(&pm);
        let rst_handler: Arc<PioFn> =
            Arc::new(move |_offset: u16, rwo: RWOp<'_>| {
                pm_rst.handle_reset(rwo);
            });
        if let Err(e) = bus_pio.register(0xCF9, 1, rst_handler) {
            slog::warn!(pm.log, "failed to register reset handler"; "error" => %e);
        }

        // SMI Command port at 0xB2 (ACPI enable/disable)
        let pm_smi = Arc::clone(&pm);
        let smi_handler: Arc<PioFn> =
            Arc::new(move |_offset: u16, rwo: RWOp<'_>| {
                pm_smi.handle_smi_cmd(rwo);
            });
        if let Err(e) = bus_pio.register(SMI_CMD_PORT, 1, smi_handler) {
            slog::warn!(pm.log, "failed to register SMI command handler"; "error" => %e);
        }

        pm
    }

    /// Handle PM1 Event block I/O (status at offset 0, enable at offset 2).
    fn handle_pm1_evt(&self, offset: u16, rwo: RWOp<'_>) {
        let mut inner = self.inner.lock().expect("pm lock poisoned");

        match (offset, rwo) {
            // PM1 Status read (offset 0, 2 bytes)
            (0, RWOp::Read(ro)) => {
                ro.write_u16(inner.pm1_status);
            }
            // PM1 Status write (offset 0, write-1-to-clear)
            (0, RWOp::Write(wo)) => {
                let val = wo.read_u16();
                inner.pm1_status &= !(val & PM1_STS_CLEARABLE);
            }
            // PM1 Enable read (offset 2)
            (2, RWOp::Read(ro)) => {
                ro.write_u16(inner.pm1_enable);
            }
            // PM1 Enable write (offset 2)
            (2, RWOp::Write(wo)) => {
                let val = wo.read_u16();
                inner.pm1_enable = val & PM1_EN_WRITABLE;
            }
            (_, RWOp::Read(ro)) => {
                // Byte-width access or odd offsets — return 0
                ro.write_u8(0);
            }
            _ => {
                // Ignore other writes
            }
        }
    }

    /// Handle PM1 Control block I/O.
    fn handle_pm1_cnt(&self, _offset: u16, rwo: RWOp<'_>) {
        let mut inner = self.inner.lock().expect("pm lock poisoned");

        match rwo {
            RWOp::Read(ro) => {
                ro.write_u16(inner.pm1_control);
            }
            RWOp::Write(wo) => {
                let val = wo.read_u16();
                // Preserve SCI_EN (OSPM cannot change it), clear
                // reserved bits, and don't store SLP_EN.
                inner.pm1_control = (inner.pm1_control & PM1_SCI_EN)
                    | (val & !(PM1_SLP_EN | PM1_ALWAYS_ZERO));

                // Check for sleep/poweroff request
                if val & PM1_SLP_EN != 0 {
                    let slp_type = (inner.pm1_control & PM1_SLP_TYP_MASK) >> 10;
                    if slp_type == 5 {
                        // S5 = power off. Signal via SLP_EN write.
                        // The vCPU run loop will see this as a suspend.
                        // S5 = power off
                    }
                }
            }
        }
    }

    /// Handle SMI Command register (0xB2) for ACPI enable/disable.
    fn handle_smi_cmd(&self, rwo: RWOp<'_>) {
        if let RWOp::Write(wo) = rwo {
            let val = wo.read_u8();
            let mut inner = self.inner.lock().expect("pm lock poisoned");
            match val {
                ACPI_ENABLE => {
                    inner.pm1_control |= PM1_SCI_EN;
                }
                ACPI_DISABLE => {
                    inner.pm1_control &= !PM1_SCI_EN;
                }
                _ => {
                    // Other SMI commands ignored
                }
            }
        }
    }

    /// Handle reset register I/O at 0xCF9.
    fn handle_reset(&self, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(0);
            }
            RWOp::Write(wo) => {
                let val = wo.read_u8();
                // Bit 2 = system reset when transitioning 0→1
                if val & 0x04 != 0 {
                    // System reset requested
                }
            }
        }
    }
}
