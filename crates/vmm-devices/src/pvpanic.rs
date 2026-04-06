// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! QEMU-compatible pvpanic device.
//!
//! A simple I/O port (0x505) that Linux guests write to on kernel panic.
//! The VMM logs the event and can notify the control plane.
//!
//! Guest support: `CONFIG_PVPANIC=y` (Linux 4.9+). The guest discovers
//! the device via ACPI (HID "QEMU0001") or auto-probe.

use std::sync::Arc;

use slog::{crit, Logger};

use crate::Lifecycle;

/// Default I/O port for pvpanic.
pub const PVPANIC_IOPORT: u16 = 0x505;

/// Guest has panicked.
const PVPANIC_PANICKED: u8 = 1 << 0;
/// Guest has a crash kernel loaded (kdump/kexec).
const PVPANIC_CRASH_LOADED: u8 = 1 << 1;

/// Supported events bitmask (returned on read).
const PVPANIC_SUPPORTED: u8 = PVPANIC_PANICKED | PVPANIC_CRASH_LOADED;

/// PvPanic device — handles guest panic notifications.
pub struct PvPanic {
    log: Logger,
}

impl PvPanic {
    pub fn new(log: Logger) -> Arc<Self> {
        Arc::new(Self { log })
    }

    /// Handle a PIO read from the pvpanic port.
    /// Returns the supported events bitmask.
    pub fn pio_read(&self) -> u8 {
        PVPANIC_SUPPORTED
    }

    /// Handle a PIO write to the pvpanic port.
    /// The guest writes event bits to signal a panic.
    pub fn pio_write(&self, val: u8) {
        if val & PVPANIC_PANICKED != 0 {
            crit!(self.log, "GUEST PANIC DETECTED via pvpanic";
                "event" => format!("{:#04x}", val),
            );
        }
        if val & PVPANIC_CRASH_LOADED != 0 {
            slog::info!(self.log, "guest has crash kernel loaded (kdump)");
        }
    }
}

impl Lifecycle for PvPanic {
    fn type_name(&self) -> &'static str {
        "pvpanic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_logger() -> Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn supported_events() {
        assert_eq!(PVPANIC_SUPPORTED, 0x03);
    }

    #[test]
    fn constants_are_distinct() {
        assert_eq!(PVPANIC_PANICKED & PVPANIC_CRASH_LOADED, 0);
    }

    #[test]
    fn pio_read_returns_supported_mask() {
        let dev = PvPanic::new(test_logger());
        let val = dev.pio_read();
        assert_eq!(val, PVPANIC_SUPPORTED);
        assert_eq!(val, 0x03);
    }

    #[test]
    fn pio_read_is_idempotent() {
        let dev = PvPanic::new(test_logger());
        // Multiple reads should always return the same value
        assert_eq!(dev.pio_read(), dev.pio_read());
    }

    #[test]
    fn pio_write_panicked_does_not_crash() {
        let dev = PvPanic::new(test_logger());
        // Writing PANICKED should log but not panic or crash
        dev.pio_write(PVPANIC_PANICKED);
    }

    #[test]
    fn pio_write_crash_loaded_does_not_crash() {
        let dev = PvPanic::new(test_logger());
        dev.pio_write(PVPANIC_CRASH_LOADED);
    }

    #[test]
    fn pio_write_both_flags() {
        let dev = PvPanic::new(test_logger());
        dev.pio_write(PVPANIC_PANICKED | PVPANIC_CRASH_LOADED);
    }

    #[test]
    fn pio_write_zero_is_noop() {
        let dev = PvPanic::new(test_logger());
        dev.pio_write(0);
    }

    #[test]
    fn pio_write_unknown_bits_does_not_crash() {
        let dev = PvPanic::new(test_logger());
        // Guest could write any value; ensure we don't panic on unknown bits
        dev.pio_write(0xFF);
    }

    #[test]
    fn ioport_constant() {
        assert_eq!(PVPANIC_IOPORT, 0x505);
    }
}
