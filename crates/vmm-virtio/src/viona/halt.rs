// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Giving the in-kernel link back before the VM is destroyed.

use std::io::PipeWriter;
use std::thread::JoinHandle;

use vmm_devices::Lifecycle;

use viona_api::LinkOps;

use super::{RingState, VirtioViona, NET_NUM_QUEUES};

/// The interrupt poll thread, while one is running.
pub(super) struct Poller {
    /// Write end of the pipe that stops the thread.
    ///
    /// The thread parks in `poll(2)` on the viona descriptor with no
    /// timeout, so only a second descriptor can wake it. Closing this
    /// end is what makes that descriptor ready.
    pub(super) wake: PipeWriter,
    pub(super) thread: JoinHandle<()>,
}

/// Wake a running poll thread and wait for it to leave.
///
/// Closing the write end is the wake: the thread parks in an untimed
/// `poll(2)`, so nothing else reaches it.
pub(super) fn stop_poller(poller: Option<Poller>, log: &slog::Logger) {
    let Some(poller) = poller else {
        return;
    };
    drop(poller.wake);
    if poller.thread.join().is_err() {
        slog::error!(log, "the viona interrupt poll thread panicked");
    }
}

/// Reset every ring, then destroy the link.
///
/// The delete resets the rings itself, in a wait that ignores signals
/// (`viona_main.c`, `viona_ioc_delete`). A ring that is in its reset
/// state already leaves that wait at once (`viona_ring.c`,
/// `viona_ring_reset`), so each reset that succeeds here is one wait
/// the delete does not make. Nothing in this process signals the
/// teardown thread, so the order does not make the halt interruptible.
///
/// A failure is reported and the sweep carries on. Teardown must reach
/// the destroy whatever the link does.
pub(super) fn halt_link(link: &dyn LinkOps, log: &slog::Logger) {
    for ring in 0..NET_NUM_QUEUES as u16 {
        if let Err(e) = link.ring_reset(ring) {
            slog::warn!(log, "viona ring reset failed during halt";
                "ring" => ring, "error" => %e);
        }
    }

    // WHY this is an error and not a warning: the hold this call gives
    // back is what VM_DESTROY_SELF waits for. A delete that failed
    // predicts the destroy wedge.
    if let Err(e) = link.delete() {
        slog::error!(log, "viona link delete failed; the VM destroy may wait for its hold";
            "error" => %e);
    }
}

impl Lifecycle for VirtioViona {
    fn type_name(&self) -> &'static str {
        "virtio-viona"
    }

    /// Give the in-kernel link back before the VM is destroyed.
    ///
    /// WHY teardown cannot skip this: viona takes a `vmm_drv` hold for
    /// the life of its link. `VM_DESTROY_SELF` marks every hold for
    /// release and then waits for the leases to break in an untimed
    /// `cv_wait` (`vmm_sol_dev.c`, `vmm_drv_purge`). Releasing the last
    /// hold first clears `VMM_HELD`, and the purge skips that wait
    /// altogether. The order below is Propolis's: stop the poller, kill
    /// the rings, then delete the link.
    ///
    /// This call is not bounded. The delete waits for each ring worker
    /// to stop, and does not heed signals, so it must run on the
    /// teardown thread the shutdown watchdog keeps a clock on.
    ///
    /// A hot unplug halts too, with the guest running
    /// (`vmm_devices::lifecycle::prepare_unplug`). So the device lock is
    /// dropped before any of that: `cfg_read`, `notify_queue`,
    /// `queue_addr_set` and `reset` take the same lock, and a vCPU in
    /// one of them must not wait out an untimed kernel call.
    fn halt(&self) {
        let (link, log, poller) = {
            let mut inner = self.inner.lock().expect("viona lock poisoned");
            // A second halt has no link left to destroy. viona would
            // treat it as a no-op, but a repeated delete would also
            // repeat the wait inside it.
            if inner.halted {
                return;
            }
            inner.halted = true;
            // A deferred start held past the halt would revive a dead
            // link.
            inner.deferred_poll = None;
            // Nothing may kick the rings again: they are gone. This is
            // set before the lock is dropped, so a kick that races the
            // delete finds no ring to send.
            inner.ring_state = [RingState::Init; NET_NUM_QUEUES];
            inner.negotiated_features = 0;
            (inner.link(), inner.log(), inner.poller.take())
        };

        // The poll thread reads the viona descriptor and calls back
        // into the PCI transport, so it must be gone before the link
        // is destroyed. The join needs the device lock dropped as well:
        // the thread may be inside that callback now, and it takes
        // transport locks.
        stop_poller(poller, &log);
        halt_link(link.as_ref(), &log);
    }
}
