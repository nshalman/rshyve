// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Interrupt pin abstractions for virtual device IRQ delivery.
//!
//! Provides:
//! - [`IntrPin`] trait for abstract interrupt pin behavior
//! - [`LegacyPIC`] for ISA IRQ delivery through the kernel PIC/IOAPIC
//! - [`LegacyPin`] for individual IRQ lines on the legacy PIC
//! - [`NoOpPin`] for optional/unused interrupt signals

#![allow(clippy::mutex_atomic)]

use std::sync::{Arc, Mutex, Weak};

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
}

impl LegacyPIC {
    /// Create a new `LegacyPIC` backed by the given VM handle.
    pub fn new(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new([Entry::default(); PIN_COUNT as usize]),
            hdl,
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
        let mut pins = self
            .inner
            .lock()
            .expect("LegacyPIC lock poisoned");

        if pins[irq as usize].process_op(&op) {
            let irq_i32 = i32::from(irq);
            let result = match op {
                PinOp::Assert => {
                    self.hdl.isa_assert_irq(irq_i32, irq_i32)
                }
                PinOp::Deassert => {
                    self.hdl.isa_deassert_irq(irq_i32, irq_i32)
                }
                PinOp::Pulse => {
                    self.hdl.isa_pulse_irq(irq_i32, irq_i32)
                }
            };
            if let Err(e) = result {
                // Log but don't panic -- IRQ delivery failure in the
                // kernel is not recoverable but should not crash the VMM.
                eprintln!("LegacyPIC: failed to deliver IRQ {irq}: {e}");
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
        let mut asserted = self
            .asserted
            .lock()
            .expect("LegacyPin lock poisoned");
        if !*asserted {
            *asserted = true;
            if let Some(pic) = self.pic.upgrade() {
                pic.do_irq(PinOp::Assert, self.irq);
            }
        }
    }

    fn deassert(&self) {
        let mut asserted = self
            .asserted
            .lock()
            .expect("LegacyPin lock poisoned");
        if *asserted {
            *asserted = false;
            if let Some(pic) = self.pic.upgrade() {
                pic.do_irq(PinOp::Deassert, self.irq);
            }
        }
    }

    fn pulse(&self) {
        let asserted = self
            .asserted
            .lock()
            .expect("LegacyPin lock poisoned");
        if !*asserted {
            if let Some(pic) = self.pic.upgrade() {
                pic.do_irq(PinOp::Pulse, self.irq);
            }
        }
    }

    fn is_asserted(&self) -> bool {
        *self
            .asserted
            .lock()
            .expect("LegacyPin lock poisoned")
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
