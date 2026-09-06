// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The in-kernel calls the device makes.
//!
//! Every call goes through one trait, on every platform. The illumos
//! build puts its open [`viona_api::VionaFd`] behind it, so the device
//! holds the same `Arc<dyn LinkOps>` the tests hand it. That is what
//! lets the tests run where the product runs.
//!
//! The trait covers what the callers need, not one method per ioctl.
//! Nothing outside this file names the concrete handle, so no caller
//! can reach past the trait and gate itself off the target again.

use std::io::{PipeReader, Result};

use viona_api::{vioc_ring_state, PromiscMode};

/// What one `VNA_IOC_INTR_POLL` copies out: the pending state of
/// every ring, in ring order.
pub(super) type IntrStatus = [u32; viona_api::VIONA_VQ_MAX as usize];

/// What one wait for the kernel's interrupt signal ended with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IntrWait {
    /// One or more rings have an interrupt waiting to be read.
    Pending,
    /// The halt asked the thread to leave, or the link is gone.
    /// Nothing more will arrive.
    Stopped,
}

/// The kernel side of the device.
pub(super) trait LinkOps: Send + Sync {
    /// Program one ring's addresses, in the modern layout.
    ///
    /// The kernel starts a worker for the ring, so nothing may call
    /// this for a link the halt destroyed. The ring indices go back to
    /// 0, which is why a migration destination needs
    /// [`Self::ring_set_state`] instead.
    fn ring_init(
        &self,
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> Result<()>;

    /// Tell the kernel worker to look at one ring again.
    fn ring_kick(&self, ring: u16) -> Result<()>;

    /// Return one ring to its reset state.
    fn ring_reset(&self, ring: u16) -> Result<()>;

    /// Stop one ring's worker and keep its state.
    ///
    /// A migration source pauses each ring before it reads the indices
    /// out, so the kernel does not move them again.
    fn ring_pause(&self, ring: u16) -> Result<()>;

    /// Program one ring's addresses and its indices together.
    ///
    /// A migration destination resumes the kernel where the source
    /// stopped.
    fn ring_set_state(&self, state: &vioc_ring_state) -> Result<()>;

    /// Read one ring's addresses and indices.
    ///
    /// A migration source writes the indices into the payload the
    /// destination resumes from.
    fn ring_get_state(&self, ring: u16) -> Result<vioc_ring_state>;

    /// Route one ring's interrupt to an MSI-X message.
    fn ring_set_msi(&self, ring: u16, addr: u64, msg: u64) -> Result<()>;

    /// Give the kernel the features the guest accepted.
    fn set_features(&self, features: u64) -> Result<()>;

    /// Read which rings have an interrupt pending.
    ///
    /// One entry per ring, non-zero for a ring the kernel has work
    /// waiting on.
    fn intr_status(&self) -> Result<IntrStatus>;

    /// Clear one ring's pending interrupt.
    ///
    /// Until this runs the kernel signals the same interrupt again
    /// and the poll thread spins. The clear is also what makes the
    /// notification the poll thread's to deliver.
    fn ring_intr_clr(&self, ring: u16) -> Result<()>;

    /// Wait until the kernel has interrupts pending, or the halt
    /// closes the write end of `stop`.
    ///
    /// The wait has no deadline, so that descriptor is the only thing
    /// that ends it on demand.
    fn wait_intr(&self, stop: &PipeReader, log: &slog::Logger) -> IntrWait;

    /// Name the I/O port the guest writes to kick a ring.
    fn set_notify_iop(&self, port: u16) -> Result<()>;

    /// Name the memory address the guest writes to kick a ring.
    fn set_notify_mmio(&self, addr: u64, size: u32) -> Result<()>;

    /// Set what traffic the link passes to the guest.
    fn set_promisc(&self, mode: PromiscMode) -> Result<()>;

    /// Destroy the link, which gives back its `vmm_drv` hold.
    fn delete(&self) -> Result<()>;
}

/// The link a VM runs on.
///
/// Not gated on illumos: the handle can only be opened there, and the
/// portable build gets to lint and type-check the adapter that the
/// product uses.
impl LinkOps for viona_api::VionaFd {
    fn ring_init(
        &self,
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> Result<()> {
        viona_api::VionaFd::ring_init_modern(
            self, ring, size, desc, avail, used,
        )
    }

    fn ring_kick(&self, ring: u16) -> Result<()> {
        viona_api::VionaFd::ring_kick(self, ring as usize)
    }

    fn ring_reset(&self, ring: u16) -> Result<()> {
        viona_api::VionaFd::ring_reset(self, ring as usize)
    }

    fn ring_pause(&self, ring: u16) -> Result<()> {
        viona_api::VionaFd::ring_pause(self, ring as usize)
    }

    fn ring_set_state(&self, state: &vioc_ring_state) -> Result<()> {
        viona_api::VionaFd::ring_set_state(self, state)
    }

    fn ring_get_state(&self, ring: u16) -> Result<vioc_ring_state> {
        viona_api::VionaFd::ring_get_state(self, ring)
    }

    fn ring_set_msi(&self, ring: u16, addr: u64, msg: u64) -> Result<()> {
        viona_api::VionaFd::ring_set_msi(self, ring, addr, msg)
    }

    fn set_features(&self, features: u64) -> Result<()> {
        viona_api::VionaFd::set_features(self, features)
    }

    fn intr_status(&self) -> Result<IntrStatus> {
        viona_api::VionaFd::intr_status(self)
    }

    fn ring_intr_clr(&self, ring: u16) -> Result<()> {
        viona_api::VionaFd::ring_intr_clr(self, ring as usize)
    }

    /// Wait for `POLLRDBAND`, viona's signal that interrupts are
    /// pending.
    fn wait_intr(&self, stop: &PipeReader, log: &slog::Logger) -> IntrWait {
        use std::os::fd::AsRawFd;

        loop {
            let mut pfds = [
                libc::pollfd {
                    fd: self.as_raw_fd(),
                    events: libc::POLLRDBAND,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stop.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // Safety: both descriptors are open for this call, and the
            // count matches the array.
            let ret = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                // The device is closed, or the error is fatal.
                slog::warn!(log, "the viona interrupt wait failed";
                    "error" => %err);
                return IntrWait::Stopped;
            }
            if ret == 0 {
                continue;
            }

            // The halt closed its end of the pipe, so this descriptor
            // is ready and stays ready. Leave before touching the link.
            if pfds[1].revents != 0 {
                return IntrWait::Stopped;
            }
            let revents = pfds[0].revents;
            if revents & libc::POLLRDBAND != 0 {
                return IntrWait::Pending;
            }
            if revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                return IntrWait::Stopped;
            }
        }
    }

    fn set_notify_iop(&self, port: u16) -> Result<()> {
        viona_api::VionaFd::set_notify_iop(self, port)
    }

    fn set_notify_mmio(&self, addr: u64, size: u32) -> Result<()> {
        viona_api::VionaFd::set_notify_mmio(self, addr, size)
    }

    fn set_promisc(&self, mode: PromiscMode) -> Result<()> {
        viona_api::VionaFd::set_promisc(self, mode)
    }

    fn delete(&self) -> Result<()> {
        viona_api::VionaFd::delete(self)
    }
}
