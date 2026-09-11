// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The bounded halt shared by the devices that serve a unix socket.
//!
//! # Invariant
//!
//! Halt returns inside the budget it declares, and nothing in it waits
//! on a host peer. Each join is bounded by the budget, not tested
//! before it starts, so one wedged thread cannot hold teardown open.
//! Every thread ends on the shutdown flag, so halt never connects to
//! its own socket to wake the accept thread: a same-uid peer that
//! fills the listen backlog can make that connect wait forever.

use std::path::Path;
use std::thread::JoinHandle;
use std::time::Duration;

use slog::{debug, Logger};

/// How long one set of threads gets to end.
///
/// The teardown sweep halts every device before `VM_DESTROY_SELF` and
/// gives each halt a budget of its own. Staying inside that budget
/// keeps the sweep from abandoning a halt part-way.
pub(crate) const HALT_BUDGET: Duration = Duration::from_secs(2);

/// How many joins run one after the other: the accept thread, then
/// the readers.
pub(crate) const HALT_JOIN_ROUNDS: u32 = 2;

/// The deadline teardown must allow: the product, not the constant.
pub(crate) fn halt_budget() -> Duration {
    HALT_BUDGET.saturating_mul(HALT_JOIN_ROUNDS)
}

/// End the accept thread, then the peers and their readers, then the
/// socket file.
///
/// The caller has already raised its shutdown flag. `disconnect` shuts
/// every peer socket down, which wakes the readers blocked in
/// `read(2)`, and hands back their threads. It runs after the accept
/// thread has returned, so no reader is added behind it. A reader the
/// shutdown did not reach sits in a read nothing here can revoke, so it
/// is left running and teardown goes on.
pub(crate) fn halt_socket_device(
    log: &Logger,
    device: &str,
    accept: Option<JoinHandle<()>>,
    disconnect: impl FnOnce() -> Vec<JoinHandle<()>>,
    socket_path: &Path,
) {
    if let Some(handle) = accept {
        if !vmm_core::thread::join_bounded(vec![handle], HALT_BUDGET) {
            debug!(log, "halt left the accept thread running";
                "device" => device);
        }
    }

    if !vmm_core::thread::join_bounded(disconnect(), HALT_BUDGET) {
        debug!(log, "halt left a reader running"; "device" => device);
    }

    // The socket may already be gone if the path was reused.
    let _ = std::fs::remove_file(socket_path);
}
