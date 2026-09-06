// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The interrupt poll thread.
//!
//! Viona completes I/O in-kernel and signals readiness on the device
//! descriptor. One thread per device waits for that signal and raises
//! the guest interrupt. The halt stops the thread before it destroys
//! the link, because the thread reads the link.
//!
//! The wait itself is a link call ([`LinkOps::wait_intr`]), so the
//! whole loop is the same code on every platform and a test can drive
//! it with its own link.

use std::sync::{Arc, Mutex, PoisonError};

use viona_api::{IntrWait, LinkOps};

use super::{VionaInterrupt, VirtioViona, NET_NUM_QUEUES};

/// The interrupt slot the poll thread reads its path from.
type IntrSlot = Arc<Mutex<Option<VionaInterrupt>>>;

// The poll thread raises one interrupt per ring, and the transport
// holds one MSI-X vector per queue. A kernel that grew more rings than
// this device has queues would raise on a queue with no vector, which
// on an MSI-X guest is silence and on INTx is a line nobody lowers.
const _: () = assert!(viona_api::VIONA_VQ_MAX as usize == NET_NUM_QUEUES);

impl VirtioViona {
    /// Start the interrupt polling thread.
    ///
    /// Viona completes I/O in-kernel and signals readiness on the
    /// device fd. This thread waits for that signal, discovers which
    /// queues have pending interrupts, clears them, and raises the
    /// per-queue interrupt so the guest processes the used ring.
    ///
    /// The raise carries the queue index (0=rx, 1=tx) so the PCI
    /// transport can route via the correct MSI-X vector.
    pub fn start_intr_poll(&self, log: slog::Logger) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.log = Some(log.clone());
        // A halted device has no link left to poll.
        if inner.halted {
            return;
        }
        let intr = Arc::clone(&self.interrupt);
        Self::poller_start(&mut inner, intr, log);
    }

    /// Spawn the interrupt poll thread and record it for the halt.
    ///
    /// The spawn failures panic, as they did before the halt existed: a
    /// viona device with no poll thread delivers no interrupt to the
    /// guest, and both failures are resource exhaustion at boot.
    fn poller_start(
        inner: &mut super::VionaInner,
        intr: IntrSlot,
        log: slog::Logger,
    ) {
        // A second thread on the same descriptor would be left running
        // when the halt stops the first one.
        if inner.poller.is_some() {
            slog::warn!(
                log,
                "a viona interrupt poll thread is running already"
            );
            return;
        }

        // Without this pipe the thread could not be stopped, and the
        // halt would join a thread parked in an untimed wait.
        let (stop, wake) =
            std::io::pipe().expect("failed to open the viona poll stop pipe");
        // The thread holds the link, not a bare descriptor: a device
        // dropped without a halt would otherwise close the fd under it.
        let link = inner.link();
        let thread = std::thread::Builder::new()
            .name("viona-intr-poll".into())
            .spawn(move || {
                viona_intr_poll_loop(link.as_ref(), stop, &intr, log);
            })
            .expect("failed to spawn viona-intr-poll thread");
        inner.poller = Some(super::halt::Poller { wake, thread });
    }

    /// Hold the logger for a deferred start of the poll thread.
    ///
    /// On migration destinations, the poll thread cannot start at device
    /// creation time because the rings haven't been programmed yet
    /// (and starting it would make them EBUSY). Instead, store the
    /// logger here, then call `start_poll_deferred()` after ring state
    /// is fully restored.
    pub fn defer_intr_poll(&self, log: slog::Logger) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.log = Some(log.clone());
        // A halted device has no link left to poll.
        if inner.halted {
            return;
        }
        inner.deferred_poll = Some(log);
    }

    /// Start the interrupt poll thread using previously stored parameters.
    ///
    /// Called after migration restore has programmed all ring state,
    /// MSI-X vectors, and notification addresses. This is the final
    /// step that enables viona to deliver completion interrupts to
    /// the guest.
    pub fn start_poll_deferred(&self) {
        use super::RingState;

        let mut inner = self.inner.lock().expect("viona lock");
        // A migration restore that lands after the halt must not start
        // a thread on a link that is already destroyed.
        if inner.halted {
            inner.deferred_poll = None;
            return;
        }
        let Some(log) = inner.deferred_poll.take() else {
            return;
        };
        slog::info!(log, "starting deferred viona interrupt poll thread");
        let intr = Arc::clone(&self.interrupt);
        Self::poller_start(&mut inner, intr, log);

        // Re-kick all rings after starting the poll thread. Programming
        // the ring state sets the indices but may not activate the
        // kernel ring worker. A second kick after the poll thread is
        // running makes the worker process available entries.
        for ring in 0..NET_NUM_QUEUES as u16 {
            if inner.ring_state[ring as usize] != RingState::Ready {
                continue;
            }
            if let Err(e) = inner.link_ref().ring_kick(ring) {
                slog::warn!(inner.log(), "viona re-kick failed after restore";
                    "ring" => ring, "error" => %e);
            }
        }
    }
}

/// Deliver the interrupts one poll wakeup found pending.
///
/// WHY the session is read first: the clear below is what makes a
/// notification this thread's to deliver, and a whole device reset can
/// run between that clear and the raise. Viona's reset returns the
/// kernel rings, it does not stop this thread, so the thread comes back
/// into a session the next driver owns. A session sampled after the
/// clear names that driver, and the transport would then write an
/// interrupt for a ring it never armed: on INTx a level nobody lowers,
/// on MSI-X a message to whichever vector it had just programmed.
/// Sampled before, the same notification is refused, which is right.
/// It belongs to a driver that no longer exists.
pub(super) fn deliver_pending(
    link: &dyn LinkOps,
    slot: &Mutex<Option<VionaInterrupt>>,
    log: &slog::Logger,
) {
    // Cloned out, because the raise takes transport locks and the
    // binary fills this slot once.
    let path = slot.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let Some(intr) = path else {
        // The transport is not wired up yet, so no driver can have
        // armed a ring.
        return;
    };
    let session = intr.session();

    let status = match link.intr_status() {
        Ok(status) => status,
        Err(err) => {
            slog::warn!(log, "VNA_IOC_INTR_POLL failed"; "error" => %err);
            return;
        }
    };

    // Fire per ring so the PCI transport routes via the correct MSI-X
    // vector, or falls back to shared INTx.
    for (ring, pending) in status.iter().enumerate() {
        if *pending == 0 {
            continue;
        }
        let ring = ring as u16;
        // Until this clears, the kernel signals the same interrupt
        // again and this loop spins.
        if let Err(err) = link.ring_intr_clr(ring) {
            slog::warn!(log, "viona interrupt clear failed";
                "ring" => ring, "error" => %err);
        }
        intr.raise(session, ring);
    }
}

/// Interrupt polling loop for the viona kernel driver.
///
/// Each wakeup the link reports pending is handed to
/// [`deliver_pending`].
///
/// `stop` is the read end of the halt's pipe. The wait has no timeout,
/// so that descriptor is the only thing that ends this loop on demand.
fn viona_intr_poll_loop(
    link: &dyn LinkOps,
    stop: std::io::PipeReader,
    slot: &Mutex<Option<VionaInterrupt>>,
    log: slog::Logger,
) {
    loop {
        match link.wait_intr(&stop) {
            Ok(IntrWait::Pending) => deliver_pending(link, slot, &log),
            // The halt closed the stop pipe, or the link is gone.
            Ok(IntrWait::Stopped) => return,
            // The descriptor is closed, or the error is fatal. Waiting
            // again would spin on the same error.
            Err(err) => {
                slog::warn!(log, "the viona interrupt wait failed";
                    "error" => %err);
                return;
            }
        }
    }
}
