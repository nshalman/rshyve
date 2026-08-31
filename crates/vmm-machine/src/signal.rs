// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SIGTERM handling.
//!
//! The static, the `extern "C"` handler that writes it, and the poller
//! that reads it live together: a `static` cannot be split across a
//! crate boundary, and a signal handler cannot capture.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use slog::{info, Logger};

use vmm_core::machine::Machine;

/// Atomic flag set by the SIGTERM signal handler.
static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Signal handler for SIGTERM. It only sets the atomic flag. The
/// shutdown runs in the poller thread, because a handler may call only
/// async-signal-safe functions.
extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::Relaxed);
}

/// Borrow the flag the handler writes.
///
/// A caller that must re-read the flag at a later decision point takes
/// the borrow. A caller that only needs the value now calls
/// [`sigterm_received`].
pub fn sigterm_flag() -> &'static AtomicBool {
    &SIGTERM_RECEIVED
}

/// Whether the SIGTERM handler has fired.
pub fn sigterm_received() -> bool {
    SIGTERM_RECEIVED.load(Ordering::Relaxed)
}

/// Install the SIGTERM signal handler and spawn a poller thread that
/// triggers ACPI poweroff when the signal is received.
pub fn install_sigterm_handler(machine: &Machine, log: &Logger) {
    let log_sig = log.clone();
    // libc::signal returns SIG_ERR on failure; safe to ignore
    let _ = unsafe {
        libc::signal(
            libc::SIGTERM,
            sigterm_handler as *const () as libc::sighandler_t,
        )
    };
    // Spawn a thread to wait for the signal flag
    let hdl_sig = machine.hdl().clone();
    thread::Builder::new()
        .name("sigterm-handler".into())
        .spawn(move || loop {
            thread::sleep(std::time::Duration::from_millis(500));
            if sigterm_received() {
                info!(log_sig, "SIGTERM received, triggering ACPI poweroff");
                // best-effort: if suspend fails, VM is already gone
                let _ = hdl_sig.suspend(
                    bhyve_api::vm_suspend_how::VM_SUSPEND_POWEROFF as u32,
                    0,
                );
                break;
            }
        })
        .expect("failed to spawn sigterm handler thread");
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::{
        sigterm_flag, sigterm_handler, sigterm_received, SIGTERM_RECEIVED,
    };

    /// One test owns the whole flag, because a second test that wrote it
    /// would race this one.
    #[test]
    fn handler_write_is_visible_through_both_accessors() {
        // A static cannot be split across a crate boundary, and the
        // handler cannot capture, so the accessors must hand back the
        // same cell the handler writes.
        assert!(std::ptr::eq(sigterm_flag(), &SIGTERM_RECEIVED));
        assert!(!sigterm_received());

        sigterm_handler(libc::SIGTERM);

        assert!(sigterm_received());
        assert!(sigterm_flag().load(Ordering::Relaxed));
        SIGTERM_RECEIVED.store(false, Ordering::Relaxed);
    }
}
