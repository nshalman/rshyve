// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! virtio-vsock: multiplexed streams between host and guest.

pub mod conn;
pub mod control;
pub mod device;
pub mod mux;
pub mod packet;

use std::sync::atomic::{AtomicBool, Ordering};

/// Cleared by the privilege lock. A connect to `<socket>_<port>` needs
/// `file_write` on illumos (`VOP_ACCESS(VREAD|VWRITE)`), which the
/// locked floor does not hold, so every guest connect would fail.
static GUEST_CONNECTS: AtomicBool = AtomicBool::new(true);

/// Refuse every later guest-initiated connect, with one warning per
/// device instead of a failed connect per packet.
pub fn deny_guest_connects() {
    GUEST_CONNECTS.store(false, Ordering::Release);
}

pub(crate) fn guest_connects_allowed() -> bool {
    GUEST_CONNECTS.load(Ordering::Acquire)
}
