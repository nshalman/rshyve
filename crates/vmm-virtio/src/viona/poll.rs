// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The interrupt poll thread.
//!
//! Viona completes I/O in-kernel and signals readiness on the device
//! descriptor. One thread per device waits for that signal and raises
//! the guest interrupt. The halt stops the thread before it destroys
//! the link, because the thread reads the link.

use super::{QueueInterrupt, VirtioViona};

impl VirtioViona {
    /// Start the interrupt polling thread.
    ///
    /// Viona completes I/O in-kernel and signals readiness via
    /// `POLLRDBAND` on the device fd. This thread polls for that
    /// event, calls `VNA_IOC_INTR_POLL` to discover which queues
    /// have pending interrupts, clears them, and raises the per-queue
    /// interrupt so the guest processes the used ring.
    ///
    /// The callback receives the queue index (0=rx, 1=tx) so the
    /// PCI transport can route via the correct MSI-X vector.
    #[cfg(target_os = "illumos")]
    pub fn start_intr_poll(
        &self,
        raise_intr: QueueInterrupt,
        log: slog::Logger,
    ) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.log = Some(log.clone());
        // A halted device has no link left to poll.
        if inner.halted {
            return;
        }
        Self::poller_start(&mut inner, raise_intr, log);
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn start_intr_poll(
        &self,
        _raise_intr: QueueInterrupt,
        log: slog::Logger,
    ) {
        self.inner.lock().expect("viona lock").log = Some(log);
    }

    /// Spawn the interrupt poll thread and record it for the halt.
    ///
    /// The spawn failures panic, as they did before the halt existed: a
    /// viona device with no poll thread delivers no interrupt to the
    /// guest, and both failures are resource exhaustion at boot.
    #[cfg(target_os = "illumos")]
    fn poller_start(
        inner: &mut super::VionaInner,
        raise_intr: QueueInterrupt,
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
        // halt would join a thread parked in an untimed poll(2).
        let (stop, wake) =
            std::io::pipe().expect("failed to open the viona poll stop pipe");
        // The thread holds the link, not a bare descriptor: a device
        // dropped without a halt would otherwise close the fd under it.
        let dev = std::sync::Arc::clone(&inner.viona_fd);
        let thread = std::thread::Builder::new()
            .name("viona-intr-poll".into())
            .spawn(move || {
                viona_intr_poll_loop(&dev, stop, raise_intr, log);
            })
            .expect("failed to spawn viona-intr-poll thread");
        inner.poller = Some(super::halt::Poller { wake, thread });
    }

    /// Store the interrupt poll parameters for deferred start.
    ///
    /// On migration destinations, the poll thread cannot start at device
    /// creation time because the rings haven't been programmed yet
    /// (and starting it would make them EBUSY). Instead, store the
    /// raise_fn and logger here, then call `start_poll_deferred()` after
    /// ring state is fully restored.
    pub fn store_poll_fn(&self, raise_intr: QueueInterrupt, log: slog::Logger) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.log = Some(log.clone());
        // A halted device has no link left to poll.
        if inner.halted {
            return;
        }
        inner.deferred_poll = Some((raise_intr, log));
    }

    /// Start the interrupt poll thread using previously stored parameters.
    ///
    /// Called after migration restore has programmed all ring state,
    /// MSI-X vectors, and notification addresses. This is the final
    /// step that enables viona to deliver completion interrupts to
    /// the guest.
    #[cfg(target_os = "illumos")]
    pub fn start_poll_deferred(&self) {
        use super::{RingState, NET_NUM_QUEUES};

        let mut inner = self.inner.lock().expect("viona lock");
        // A migration restore that lands after the halt must not start
        // a thread on a link that is already destroyed.
        if inner.halted {
            inner.deferred_poll = None;
            return;
        }
        if let Some((raise_intr, log)) = inner.deferred_poll.take() {
            slog::info!(log, "starting deferred viona interrupt poll thread");
            Self::poller_start(&mut inner, raise_intr, log);

            // Re-kick all rings after starting the poll thread.
            // VNA_IOC_RING_SET_STATE programs indices but may not
            // activate the kernel ring worker. A second kick after
            // the poll thread is running ensures the worker processes
            // available entries.
            for idx in 0..NET_NUM_QUEUES as u16 {
                if inner.ring_state[idx as usize] != RingState::Ready {
                    continue;
                }
                if let Err(e) = inner.link_ref().ring_kick(idx) {
                    slog::warn!(inner.log(), "viona re-kick failed after restore";
                        "ring" => idx, "error" => %e);
                }
            }
        }
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn start_poll_deferred(&self) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.deferred_poll.take();
    }
}

/// Interrupt polling loop for the viona kernel driver.
///
/// Uses `poll(2)` with `POLLRDBAND` to wait for the kernel to signal
/// that completed I/O is ready. When signaled, reads the interrupt
/// bitmap via `VNA_IOC_INTR_POLL`, clears each pending queue's
/// interrupt state, and calls the provided callback to raise the
/// PCI interrupt.
///
/// `stop` is the read end of the halt's pipe. The wait has no timeout,
/// so that descriptor is the only thing that ends this loop on demand.
#[cfg(target_os = "illumos")]
fn viona_intr_poll_loop(
    dev: &viona_api::VionaFd,
    stop: std::io::PipeReader,
    raise_intr: QueueInterrupt,
    log: slog::Logger,
) {
    use std::os::fd::AsRawFd;

    let fd = dev.as_raw_fd();
    loop {
        // Wait for POLLRDBAND, viona's signal that interrupts are pending.
        let mut pfds = [
            libc::pollfd {
                fd,
                events: libc::POLLRDBAND,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // The device is closed, or the error is fatal. Leave.
            slog::warn!(log, "poll error"; "error" => %err);
            break;
        }
        if ret == 0 {
            continue;
        }

        // The halt closed its end of the pipe, so the descriptor is
        // ready and stays ready. Leave before touching the link.
        if pfds[1].revents != 0 {
            break;
        }

        let pfd = pfds[0];
        if pfd.revents & libc::POLLRDBAND == 0 {
            // Spurious wakeup or POLLHUP/POLLERR
            if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                break;
            }
            continue;
        }

        // Read which queues have pending interrupts.
        //
        // Safety: the command copies out one vioc_intr_poll, and that
        // is what `poll_data` is.
        let mut poll_data = viona_api::vioc_intr_poll::default();
        let rc =
            unsafe { dev.ioctl(viona_api::VNA_IOC_INTR_POLL, &mut poll_data) };
        if let Err(err) = rc {
            slog::warn!(log, "VNA_IOC_INTR_POLL failed"; "error" => %err);
            continue;
        }

        // Process each queue with a pending interrupt.
        // Fire per-queue so PCI transport routes via the correct
        // MSI-X vector (or falls back to shared INTx).
        for i in 0..viona_api::VIONA_VQ_MAX as usize {
            if poll_data.vip_status[i] == 0 {
                continue;
            }
            // Until this clears, the kernel signals the same interrupt
            // again and this loop spins.
            if let Err(err) = dev.ring_intr_clr(i) {
                slog::warn!(log, "viona interrupt clear failed";
                    "ring" => i, "error" => %err);
            }
            raise_intr(i as u16);
        }
    }
}
