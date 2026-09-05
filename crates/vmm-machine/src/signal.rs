// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SIGTERM handling.
//!
//! The static, the `extern "C"` handler that writes it, and the poller
//! that reads it live together: a `static` cannot be split across a
//! crate boundary, and a signal handler cannot capture.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use slog::{error, info, warn, Logger};

use vmm_core::hdl::VmmHdl;
use vmm_core::machine::Machine;

/// Atomic flag set by the SIGTERM signal handler.
static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

/// How often the poller reads the flag the handler writes.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long the guest gets to act on the stop request.
const GRACEFUL_BUDGET: Duration = Duration::from_secs(30);

/// How long the hard stop gets before the process is ended.
///
/// This covers a VMM that never reached its teardown at all. A
/// teardown that is running clears [`teardown_flag`] of this clock,
/// because its own budgets scale with the device count and this
/// constant cannot.
const HARD_BUDGET: Duration = Duration::from_secs(20);

/// Exit code for a VMM that outlived its own shutdown.
///
/// Distinct from the guest-caused codes `main` returns, because this
/// one says the VMM itself would not stop.
const WEDGED_EXIT: i32 = 4;

/// How a shutdown reaches the VM.
///
/// Indirected through a trait so the escalation is testable without a
/// live /dev/vmm handle, and so a VM with a control plane can stop
/// through it. Only the control plane knows whether an operator has
/// the VM paused.
pub trait GuestStop: Send + Sync + 'static {
    /// Ask the guest to power off.
    fn request_poweroff(&self) -> io::Result<()>;

    /// Stop the VM whether the guest agreed or not.
    fn halt(&self) -> io::Result<()>;
}

/// Stop backed only by the VM handle.
///
/// Right for a VM with no control socket, which is the only kind that
/// cannot be paused: both `pause` and the migration that pauses arrive
/// over that socket.
pub struct HdlStop(Arc<VmmHdl>);

impl HdlStop {
    pub fn new(hdl: Arc<VmmHdl>) -> Self {
        Self(hdl)
    }
}

impl GuestStop for HdlStop {
    fn request_poweroff(&self) -> io::Result<()> {
        self.0
            .suspend(bhyve_api::vm_suspend_how::VM_SUSPEND_POWEROFF as u32, 0)
    }

    fn halt(&self) -> io::Result<()> {
        self.0
            .suspend(bhyve_api::vm_suspend_how::VM_SUSPEND_HALT as u32, 0)
    }
}

/// Signal handler for SIGTERM. It only sets the atomic flag. The
/// shutdown runs in the poller thread, because a handler may call only
/// async-signal-safe functions.
extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::Relaxed);
}

/// Set once the event loop has ended and teardown has begun.
static TEARDOWN_STARTED: AtomicBool = AtomicBool::new(false);

/// Borrow the flag [`mark_teardown_started`] writes.
pub fn teardown_flag() -> &'static AtomicBool {
    &TEARDOWN_STARTED
}

/// Say that the guest has stopped and teardown is running.
///
/// Teardown bounds every wait it makes, so the escalation has nothing
/// left to add once this is set.
pub fn mark_teardown_started() {
    TEARDOWN_STARTED.store(true, Ordering::Release);
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

/// Run one SIGTERM shutdown to its end.
///
/// Each wait is only reached while the VMM is still alive: a VM that
/// stopped ends the event loop, `main` returns, and the process exit
/// takes this thread with it. So reaching a later step means the step
/// before it did not work.
fn shutdown_sequence(
    vm: &dyn GuestStop,
    wait: impl Fn(Duration),
    give_up: impl FnOnce(),
    teardown: &AtomicBool,
    log: &Logger,
) {
    // The first step is a suspend, not an ACPI power button: the
    // kernel turns VM_SUSPEND_POWEROFF straight into a suspend exit on
    // every vCPU, so the guest is not asked and gets no flush window.
    // The wait that follows is for the run loop to see the exit and
    // for teardown to take over, not for the guest to answer.
    if let Err(e) = vm.request_poweroff() {
        warn!(log, "the stop request did not reach the VM"; "error" => %e);
    }
    wait(GRACEFUL_BUDGET);
    if teardown.load(Ordering::Acquire) {
        return stand_down(log);
    }

    warn!(log, "the guest did not stop in time; halting it";
        "waited_secs" => GRACEFUL_BUDGET.as_secs());
    if let Err(e) = vm.halt() {
        warn!(log, "the halt did not reach the VM"; "error" => %e);
    }
    wait(HARD_BUDGET);
    if teardown.load(Ordering::Acquire) {
        return stand_down(log);
    }

    // Nothing is left to try. A node must not keep the memory of a VM
    // that will not stop, so the process goes without its teardown.
    // Unflushed device writes are power-loss equivalent and inside the
    // volatile-write-cache contract the block devices advertise.
    error!(log, "the VMM outlived its own shutdown; ending the process";
        "exit_code" => WEDGED_EXIT);
    give_up();
}

/// Leave a running teardown to finish on its own.
///
/// Teardown bounds every wait it makes, but the total is a per-device
/// flush budget times the device count, which no constant here can
/// know. Ending the process against this clock would cut a working
/// shutdown off mid flush and return an exit code `main` never
/// produces, which reads as a VMM failure on a normal stop.
fn stand_down(log: &Logger) {
    info!(
        log,
        "teardown is running; the shutdown escalation stands down"
    );
}

/// Install the SIGTERM signal handler and spawn a poller thread that
/// triggers ACPI poweroff when the signal is received.
pub fn install_sigterm_handler(machine: &Machine, log: &Logger) {
    install_sigterm_handler_with(
        Arc::new(HdlStop::new(machine.hdl().clone())),
        log,
    )
}

/// The same install over a caller-supplied stop.
pub fn install_sigterm_handler_with(stop: Arc<dyn GuestStop>, log: &Logger) {
    let log_sig = log.clone();
    // libc::signal returns SIG_ERR on failure; safe to ignore
    let _ = unsafe {
        libc::signal(
            libc::SIGTERM,
            sigterm_handler as *const () as libc::sighandler_t,
        )
    };
    thread::Builder::new()
        .name("sigterm-handler".into())
        .spawn(move || {
            while !sigterm_received() {
                thread::sleep(POLL_INTERVAL);
            }
            info!(log_sig, "SIGTERM received, stopping the guest");
            shutdown_sequence(
                stop.as_ref(),
                thread::sleep,
                || std::process::exit(WEDGED_EXIT),
                teardown_flag(),
                &log_sig,
            );
        })
        .expect("failed to spawn sigterm handler thread");
}

#[cfg(test)]
mod tests;
