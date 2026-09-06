// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Lifecycle: pause, resume, and the bounded halt.
//!
//! # Invariant
//!
//! Halt returns inside the budget it declares. Each join is bounded by
//! the budget, not tested before it starts, so one wedged thread cannot
//! hold teardown open.

use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

use slog::debug;
use vmm_devices::lifecycle::IndicatedState;
use vmm_devices::Lifecycle;

use super::VirtioVsock;

/// Budget for one set of threads to end during halt.
pub(super) const HALT_BUDGET: Duration = Duration::from_secs(2);

/// How many of those joins the halt runs one after the other: the
/// accept thread, then the readers. The deadline teardown must allow
/// is the product, not the constant.
pub(super) const HALT_JOIN_ROUNDS: u32 = 2;

impl Lifecycle for VirtioVsock {
    fn type_name(&self) -> &'static str {
        "virtio-vsock"
    }

    fn lifecycle_state(&self) -> Option<IndicatedState> {
        Some(self.indicator.state())
    }

    fn start(&self) -> anyhow::Result<()> {
        self.indicator.start();
        Ok(())
    }

    fn pause(&self) {
        self.indicator.pause();
        self.shared.gate.pause();
    }

    fn is_quiesced(&self) -> bool {
        self.shared.gate.is_quiesced()
    }

    fn resume(&self) {
        self.shared.gate.resume();
        self.indicator.resume();
        // Packets queued during the pause are owed to the guest.
        self.shared.deliver_rx();
    }

    /// The halt joins the accept thread for one budget and then the
    /// reader threads for another, one after the other, so the deadline
    /// it really holds is twice the constant.
    fn halt_budget(&self) -> Duration {
        HALT_BUDGET.saturating_mul(HALT_JOIN_ROUNDS)
    }

    fn halt(&self) {
        self.indicator.halt();
        self.shared.shutdown.store(true, Ordering::Release);

        // The accept thread is blocked in accept(). Connecting once
        // wakes it so it sees the flag; without this the join below
        // waits for a client that may never come.
        let _ = UnixStream::connect(&self.shared.socket_path);
        // Bounded for the same reason the readers are: if that connect
        // did not land, the accept thread is still blocked in accept()
        // and joining it has no end.
        if let Some(handle) = self.accept.lock().expect("accept lock").take() {
            if !vmm_core::thread::join_bounded(vec![handle], HALT_BUDGET) {
                debug!(
                    self.shared.log,
                    "virtio-vsock halt left the accept thread running"
                );
            }
        }

        // Shutting every socket down wakes the readers blocked on
        // read(). The accept thread has returned, so no reader is added
        // after this point.
        for key in self.shared.socket_keys() {
            self.shared.drop_socket(key);
        }
        // The budget has to bound the join itself. Testing a deadline
        // before each `join()` only decides whether the next one
        // starts, so one wedged reader holds teardown open for as long
        // as it stays wedged.
        if !vmm_core::thread::join_bounded(
            self.shared.take_readers(),
            HALT_BUDGET,
        ) {
            debug!(self.shared.log, "virtio-vsock halt left a reader running");
        }

        // The socket may already be gone if the path was reused.
        let _ = std::fs::remove_file(&self.shared.socket_path);
    }
}
