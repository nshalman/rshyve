// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Portions derived from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream: lib/propolis/src/intr_pins.rs
// https://github.com/oxidecomputer/propolis

//! Interrupt pin abstractions for virtual device IRQ delivery.
//!
//! Provides:
//! - [`IntrPin`] trait for abstract interrupt pin behavior
//! - [`LegacyPIC`] for ISA IRQ delivery through the kernel PIC/IOAPIC
//! - [`LegacyPin`] for individual IRQ lines on the legacy PIC
//! - [`NoOpPin`] for optional/unused interrupt signals

#![allow(clippy::mutex_atomic)]

use std::sync::{Arc, Mutex, Weak};

use slog;

use crate::hdl::VmmHdl;

/// Number of ISA IRQ lines (IRQ 0-15).
const PIN_COUNT: u8 = 16;

/// An abstract interrupt pin that can be asserted or deasserted.
///
/// Implementations deliver interrupts to the guest through
/// platform-specific mechanisms (e.g., kernel ioctls for PIC/IOAPIC).
pub trait IntrPin: Send + Sync + 'static {
    /// Assert (raise) the interrupt line.
    fn assert(&self);

    /// Deassert (lower) the interrupt line.
    fn deassert(&self);

    /// Returns whether the pin is currently asserted.
    fn is_asserted(&self) -> bool;

    /// Pulse the interrupt: assert then immediately deassert.
    ///
    /// Only pulses if the pin is not already asserted, to avoid
    /// spurious edges on a held-high line.
    fn pulse(&self) {
        if !self.is_asserted() {
            self.assert();
            self.deassert();
        }
    }

    /// Set the pin to a specific state.
    fn set_state(&self, is_asserted: bool) {
        if is_asserted {
            self.assert();
        } else {
            self.deassert();
        }
    }
}

/// Describes the operation to perform on an interrupt pin.
enum PinOp {
    /// Assert (raise) the interrupt.
    Assert,
    /// Deassert (lower) the interrupt.
    Deassert,
    /// Assert then immediately deassert.
    Pulse,
}

/// Tracks the shared level count for a single IRQ line.
///
/// Multiple devices can share an IRQ. The level count tracks how many
/// sources are currently asserting so the kernel ioctl is only called
/// on actual transitions (0->1 for assert, 1->0 for deassert).
#[derive(Default, Copy, Clone)]
struct Entry {
    level: usize,
}

impl Entry {
    /// Process an interrupt operation and return whether the kernel
    /// should be notified (i.e., whether a level transition occurred).
    fn process_op(&mut self, op: &PinOp) -> bool {
        match op {
            PinOp::Assert => {
                self.level += 1;
                // Notify on 0->1 transition
                self.level == 1
            }
            PinOp::Deassert => {
                // Saturate at zero to avoid underflow from mismatched
                // assert/deassert pairs.
                if self.level == 0 {
                    return false;
                }
                self.level -= 1;
                // Notify on 1->0 transition
                self.level == 0
            }
            PinOp::Pulse => {
                // Only pulse if line is currently low
                self.level == 0
            }
        }
    }
}

/// Virtual legacy PIC/IOAPIC that delivers ISA IRQs to the kernel.
///
/// Wraps a [`VmmHdl`] and manages 16 IRQ lines (IRQ 0-15). Each line
/// tracks a shared level count so that multiple devices can share an
/// IRQ without spurious transitions.
pub struct LegacyPIC {
    inner: Mutex<[Entry; PIN_COUNT as usize]>,
    hdl: Arc<VmmHdl>,
    log: slog::Logger,
}

impl LegacyPIC {
    /// Create a new `LegacyPIC` backed by the given VM handle.
    pub fn new(hdl: Arc<VmmHdl>, log: slog::Logger) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new([Entry::default(); PIN_COUNT as usize]),
            hdl,
            log,
        })
    }

    /// Create a pin handle for the given ISA IRQ number.
    ///
    /// Returns `None` if the IRQ is out of range (>= 16) or is IRQ 2
    /// (which is reserved for the PIC cascade and cannot be used by
    /// devices).
    pub fn pin_handle(self: &Arc<Self>, irq: u8) -> Option<Arc<LegacyPin>> {
        if irq >= PIN_COUNT || irq == 2 {
            return None;
        }
        Some(Arc::new(LegacyPin::new(irq, Arc::downgrade(self))))
    }

    /// Perform an IRQ operation, calling the kernel ioctl only on
    /// level transitions.
    fn do_irq(&self, op: PinOp, irq: u8) {
        let mut pins = self.inner.lock().expect("LegacyPIC lock poisoned");

        if pins[irq as usize].process_op(&op) {
            let irq_i32 = i32::from(irq);
            let result = match op {
                PinOp::Assert => self.hdl.isa_assert_irq(irq_i32, irq_i32),
                PinOp::Deassert => self.hdl.isa_deassert_irq(irq_i32, irq_i32),
                PinOp::Pulse => self.hdl.isa_pulse_irq(irq_i32, irq_i32),
            };
            if let Err(e) = result {
                // Log but don't panic -- IRQ delivery failure in the
                // kernel is not recoverable but should not crash the VMM.
                slog::warn!(self.log, "failed to deliver IRQ"; "irq" => irq, "error" => %e);
            }
        }
    }
}

/// A single ISA IRQ line on a [`LegacyPIC`].
///
/// Tracks per-pin asserted state so that redundant assert/deassert
/// calls from a device don't generate unnecessary kernel ioctls.
/// Holds a [`Weak`] reference to the parent PIC so the PIC can be
/// dropped independently.
pub struct LegacyPin {
    irq: u8,
    asserted: Mutex<bool>,
    pic: Weak<LegacyPIC>,
}

impl LegacyPin {
    fn new(irq: u8, pic: Weak<LegacyPIC>) -> Self {
        Self {
            irq,
            asserted: Mutex::new(false),
            pic,
        }
    }
}

impl IntrPin for LegacyPin {
    fn assert(&self) {
        let mut asserted =
            self.asserted.lock().expect("LegacyPin lock poisoned");
        if !*asserted {
            *asserted = true;
            if let Some(pic) = self.pic.upgrade() {
                pic.do_irq(PinOp::Assert, self.irq);
            }
        }
    }

    fn deassert(&self) {
        let mut asserted =
            self.asserted.lock().expect("LegacyPin lock poisoned");
        if *asserted {
            *asserted = false;
            if let Some(pic) = self.pic.upgrade() {
                pic.do_irq(PinOp::Deassert, self.irq);
            }
        }
    }

    fn pulse(&self) {
        let asserted = self.asserted.lock().expect("LegacyPin lock poisoned");
        if !*asserted {
            if let Some(pic) = self.pic.upgrade() {
                pic.do_irq(PinOp::Pulse, self.irq);
            }
        }
    }

    fn is_asserted(&self) -> bool {
        *self.asserted.lock().expect("LegacyPin lock poisoned")
    }
}

/// A no-op interrupt pin that silently discards all operations.
///
/// Useful for optional signals (e.g., power button, reset line) where
/// the interrupt is not wired to anything.
pub struct NoOpPin;

impl IntrPin for NoOpPin {
    fn assert(&self) {}
    fn deassert(&self) {}
    fn pulse(&self) {}
    fn is_asserted(&self) -> bool {
        false
    }
}

/// An interrupt pin backed by the IOAPIC for PCI GSI delivery.
///
/// Used for PCI devices whose interrupts are routed through the
/// IOAPIC (GSIs >= 16) rather than the legacy ISA PIC.
pub struct IoApicPin {
    irq: i32,
    asserted: Mutex<bool>,
    hdl: Arc<VmmHdl>,
    log: slog::Logger,
}

impl IoApicPin {
    /// Create a new IOAPIC pin for the given GSI.
    pub fn new(gsi: u8, hdl: Arc<VmmHdl>, log: slog::Logger) -> Arc<Self> {
        Arc::new(Self {
            irq: gsi as i32,
            asserted: Mutex::new(false),
            hdl,
            log,
        })
    }
}

impl IntrPin for IoApicPin {
    fn assert(&self) {
        let mut asserted =
            self.asserted.lock().expect("ioapic pin lock poisoned");
        if !*asserted {
            *asserted = true;
            if let Err(e) = self.hdl.ioapic_assert_irq(self.irq) {
                slog::warn!(self.log, "assert GSI failed"; "gsi" => self.irq, "error" => %e);
            }
        }
    }

    fn deassert(&self) {
        let mut asserted =
            self.asserted.lock().expect("ioapic pin lock poisoned");
        if *asserted {
            *asserted = false;
            // best-effort IRQ delivery via kernel ioctl
            let _ = self.hdl.ioapic_deassert_irq(self.irq);
        }
    }

    fn is_asserted(&self) -> bool {
        *self.asserted.lock().expect("ioapic pin lock poisoned")
    }
}

/// PCI interrupt pin routed through the i440fx/PIIX3 PIRQ mechanism.
///
/// Matches C bhyve's `pci_irq_assert()` behavior: calls
/// `vm_isa_assert_irq(atpic_irq, ioapic_irq)` where:
/// - `atpic_irq` is read dynamically from the PIR register (programmed
///   by firmware at config offsets 0x60-0x63 of the LPC bridge)
/// - `ioapic_irq` is fixed per device slot: `16 + (4 + slot + pin) % 8`
///
/// The PIR register is read at assertion time so that firmware can
/// reprogram routing without invalidating existing pin handles.
pub struct PciIntrPin {
    /// Index into PIR registers (0-3), from `(slot + pin) % 4`.
    pirq_idx: usize,
    /// Fixed IOAPIC pin number: `16 + (4 + slot + pin) % 8`.
    ioapic_irq: i32,
    /// Shared PIR register state (from LPC bridge config space).
    pir_regs: Arc<Mutex<[u8; 4]>>,
    /// VM handle for kernel IRQ ioctls.
    hdl: Arc<VmmHdl>,
    /// Per-pin asserted state.
    asserted: Mutex<bool>,
    /// Logger for interrupt delivery errors.
    log: slog::Logger,
}

/// PIR register bit 7: disable routing.
const PIR_DISABLE: u8 = 0x80;
/// PIR register bits 3:0: ISA IRQ number.
const PIR_IRQ_MASK: u8 = 0x0F;

impl PciIntrPin {
    /// Create a new PCI interrupt pin for a device at the given slot.
    ///
    /// # Arguments
    /// - `slot` - PCI device number (0-31)
    /// - `pin` - INTx pin (0=INTA, 1=INTB, 2=INTC, 3=INTD)
    /// - `pir_regs` - Shared reference to the LPC bridge PIR registers
    /// - `hdl` - VM handle for kernel IRQ delivery
    pub fn new(
        slot: u8,
        pin: u8,
        pir_regs: Arc<Mutex<[u8; 4]>>,
        hdl: Arc<VmmHdl>,
        log: slog::Logger,
    ) -> Arc<Self> {
        let pirq_idx = ((slot as usize) + (pin as usize)) % 4;
        let ioapic_irq = 16 + ((4 + slot as i32 + pin as i32) % 8);
        Arc::new(Self {
            pirq_idx,
            ioapic_irq,
            pir_regs,
            hdl,
            asserted: Mutex::new(false),
            log,
        })
    }

    /// Read the current ATPIC IRQ from the PIR register.
    ///
    /// Returns `Some(irq)` if the route is enabled, `None` if disabled.
    fn atpic_irq(&self) -> Option<i32> {
        let pir = self.pir_regs.lock().expect("pir lock");
        let val = pir[self.pirq_idx];
        if val & PIR_DISABLE != 0 {
            return None;
        }
        let irq = (val & PIR_IRQ_MASK) as i32;
        if irq > 0 {
            Some(irq)
        } else {
            None
        }
    }
}

impl IntrPin for PciIntrPin {
    fn assert(&self) {
        let mut asserted = self.asserted.lock().expect("pci pin lock");
        if !*asserted {
            *asserted = true;
            match self.atpic_irq() {
                Some(atpic_irq) => {
                    // PIR enabled: route through both ATPIC and IOAPIC
                    if let Err(e) =
                        self.hdl.isa_assert_irq(atpic_irq, self.ioapic_irq)
                    {
                        slog::warn!(self.log, "assert PIRQ failed";
                            "pirq_idx" => self.pirq_idx,
                            "atpic_irq" => atpic_irq,
                            "ioapic_irq" => self.ioapic_irq,
                            "error" => %e,
                        );
                    }
                }
                None => {
                    // PIR disabled (no firmware, or direct boot mode):
                    // assert IOAPIC pin directly. The guest kernel
                    // programs IOAPIC via ACPI _PRT regardless of PIR.
                    // best-effort IRQ delivery via kernel ioctl
                    let _ = self.hdl.ioapic_assert_irq(self.ioapic_irq);
                }
            }
        }
    }

    fn deassert(&self) {
        let mut asserted = self.asserted.lock().expect("pci pin lock");
        *asserted = false;
        match self.atpic_irq() {
            Some(atpic_irq) => {
                // best-effort IRQ delivery via kernel ioctl
                let _ = self.hdl.isa_deassert_irq(atpic_irq, self.ioapic_irq);
            }
            None => {
                // best-effort IRQ delivery via kernel ioctl
                let _ = self.hdl.ioapic_deassert_irq(self.ioapic_irq);
            }
        }
    }

    fn is_asserted(&self) -> bool {
        *self.asserted.lock().expect("pci pin lock")
    }
}
