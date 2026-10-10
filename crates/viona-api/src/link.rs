// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The calls a device makes on an open link.
//!
//! One trait covers the whole kernel side of a viona device, so the
//! device holds the same `Arc<dyn LinkOps>` on every platform: the open
//! [`VionaFd`] in the product, a test's own link in a test. That is
//! what lets the device tests run where the product runs.
//!
//! Each method here sends its ioctl itself. There is no second layer
//! between a method and its command, so the test that reads the ioctl
//! reads the code the device calls.
//!
//! WHY the trait lives in this crate and not beside the device. The
//! recording channel is the only handle that answers without a kernel,
//! and it is `#[cfg(test)]` in this crate. A trait implemented in
//! another crate cannot reach it, and that made the delegation
//! untestable there: eleven mutations in it kept a whole suite green.
//! Here the tests below drive these methods, and a production build
//! still cannot build a recording handle at all.

use std::io::{PipeReader, Result};
use std::os::fd::AsRawFd;

use crate::{
    vioc_intr_poll, vioc_notify_mmio, vioc_ring_init_modern, vioc_ring_msi,
    vioc_ring_state, PromiscMode, VionaFd, VIONA_VQ_MAX, VNA_IOC_DELETE,
    VNA_IOC_INTR_POLL, VNA_IOC_RING_GET_STATE, VNA_IOC_RING_INIT_MODERN,
    VNA_IOC_RING_INTR_CLR, VNA_IOC_RING_KICK, VNA_IOC_RING_PAUSE,
    VNA_IOC_RING_RESET, VNA_IOC_RING_SET_MSI, VNA_IOC_RING_SET_STATE,
    VNA_IOC_SET_FEATURES, VNA_IOC_SET_NOTIFY_IOP, VNA_IOC_SET_NOTIFY_MMIO,
    VNA_IOC_SET_PROMISC,
};

mod quiesce;
#[cfg(test)]
mod test;

pub use quiesce::{quiesce, QuiesceFailed, Quiesced, Resumed};

/// The band viona signals a pending interrupt in.
///
/// The wait asks the kernel for this band and reads the answer back
/// through the same name, so the two cannot drift apart. Ordinary
/// readability is a different thing and means nothing here.
const INTR_BAND: libc::c_short = libc::POLLRDBAND;

/// What one `VNA_IOC_INTR_POLL` copies out: the pending state of
/// every ring, in ring order.
pub type IntrStatus = [u32; VIONA_VQ_MAX as usize];

/// What one wait for the kernel's interrupt signal ended with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntrWait {
    /// One or more rings have an interrupt waiting to be read.
    Pending,
    /// The halt asked the thread to leave, or the link is gone.
    /// Nothing more will arrive.
    Stopped,
}

/// The kernel side of a viona device.
///
/// The trait covers what the callers need, not one method per ioctl.
pub trait LinkOps: Send + Sync {
    /// Program one ring's addresses, in the modern (VirtIO 1.0)
    /// layout.
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
    ///
    /// This is the guest kick. A ring that was never programmed has no
    /// worker, and the kernel refuses the call.
    fn ring_kick(&self, ring: u16) -> Result<()>;

    /// Return one ring to its reset state.
    ///
    /// [`Self::delete`] resets every ring anyway, in a wait that
    /// ignores signals. A ring that is reset already leaves that wait
    /// at once, so each reset here is one wait the delete does not
    /// make. This wait heeds signals and can return `EINTR`.
    fn ring_reset(&self, ring: u16) -> Result<()>;

    /// Stop one ring's worker and keep its state.
    ///
    /// A migration source pauses each ring before it reads the indices
    /// out, so the kernel does not move them again.
    fn ring_pause(&self, ring: u16) -> Result<()>;

    /// Program one ring's addresses and its indices together.
    ///
    /// A migration destination resumes the kernel where the source
    /// stopped, which [`Self::ring_init`] cannot do.
    fn ring_set_state(&self, state: &vioc_ring_state) -> Result<()>;

    /// Read one ring's addresses and indices.
    ///
    /// A migration source writes the indices into the payload the
    /// destination resumes from.
    fn ring_get_state(&self, ring: u16) -> Result<vioc_ring_state>;

    /// Route one ring's interrupt to an MSI-X message.
    ///
    /// The kernel then writes the message itself, and the userspace
    /// poll thread has nothing to raise for that ring.
    fn ring_set_msi(&self, ring: u16, addr: u64, msg: u64) -> Result<()>;

    /// Give the kernel the features the guest accepted.
    ///
    /// `VIRTIO_F_VERSION_1` is the one that puts the kernel rings in
    /// the modern layout.
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
    fn wait_intr(&self, stop: &PipeReader) -> Result<IntrWait>;

    /// Name the I/O port the guest writes to kick a ring, or 0 for
    /// none.
    ///
    /// The kernel then takes the kick itself, and the vCPU does not
    /// leave to userspace for it. The call takes the VM write lock, so
    /// it needs `rings` to prove no ring worker is alive.
    fn set_notify_iop(&self, rings: &Quiesced<'_>, port: u16) -> Result<()>;

    /// Name the memory window the guest writes to kick a ring. An
    /// address of 0 removes the window.
    ///
    /// The modern transport counterpart of [`Self::set_notify_iop`].
    fn set_notify_mmio(
        &self,
        rings: &Quiesced<'_>,
        addr: u64,
        size: u32,
    ) -> Result<()>;

    /// Set what traffic the link passes to the guest.
    fn set_promisc(&self, mode: PromiscMode) -> Result<()>;

    /// Destroy the link, which gives back its `vmm_drv` hold.
    ///
    /// WHY teardown must issue this: viona holds the VM for the life
    /// of the link (`viona_main.c`, `vmm_drv_hold`). `VM_DESTROY_SELF`
    /// purges the holds and then waits for every lease to break in an
    /// untimed `cv_wait` (`vmm_sol_dev.c`, `vmm_drv_purge`). Releasing
    /// the last hold first clears `VMM_HELD`, and the purge skips that
    /// wait.
    ///
    /// The call itself waits for each ring worker to stop, in a
    /// `cv_wait` that does not heed signals (`viona_ring.c`,
    /// `viona_ring_reset`). Run it only where a lost thread is
    /// acceptable.
    fn delete(&self) -> Result<()>;
}

/// The link a VM runs on.
///
/// Not gated on illumos: the handle can only be opened there, and the
/// portable build gets to lint and type-check what the product sends.
impl LinkOps for VionaFd {
    fn ring_init(
        &self,
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> Result<()> {
        let mut rim = vioc_ring_init_modern {
            rim_index: ring,
            rim_qsize: size,
            rim_qaddr_desc: desc,
            rim_qaddr_avail: avail,
            rim_qaddr_used: used,
            ..Default::default()
        };
        // Safety: the command copies in one vioc_ring_init_modern, and
        // that is what `rim` is.
        unsafe { self.ioctl(VNA_IOC_RING_INIT_MODERN, &mut rim) }?;
        Ok(())
    }

    fn ring_kick(&self, ring: u16) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_KICK, ring as usize)?;
        Ok(())
    }

    fn ring_reset(&self, ring: u16) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_RESET, ring as usize)?;
        Ok(())
    }

    fn ring_pause(&self, ring: u16) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_PAUSE, ring as usize)?;
        Ok(())
    }

    fn ring_set_state(&self, state: &vioc_ring_state) -> Result<()> {
        let mut state = *state;
        // Safety: the command copies in one vioc_ring_state, and that
        // is what `state` is.
        unsafe { self.ioctl(VNA_IOC_RING_SET_STATE, &mut state) }?;
        Ok(())
    }

    fn ring_get_state(&self, ring: u16) -> Result<vioc_ring_state> {
        let mut state = vioc_ring_state {
            vrs_index: ring,
            ..Default::default()
        };
        // Safety: the command copies out one vioc_ring_state, and that
        // is what `state` is.
        unsafe { self.ioctl(VNA_IOC_RING_GET_STATE, &mut state) }?;
        Ok(state)
    }

    fn ring_set_msi(&self, ring: u16, addr: u64, msg: u64) -> Result<()> {
        let mut msi = vioc_ring_msi {
            rm_index: ring,
            rm_addr: addr,
            rm_msg: msg,
            ..Default::default()
        };
        // Safety: the command copies in one vioc_ring_msi, and that is
        // what `msi` is.
        unsafe { self.ioctl(VNA_IOC_RING_SET_MSI, &mut msi) }?;
        Ok(())
    }

    fn set_features(&self, features: u64) -> Result<()> {
        let mut feat = features;
        // Safety: the command copies in one u64, and that is what
        // `feat` is.
        unsafe { self.ioctl(VNA_IOC_SET_FEATURES, &mut feat) }?;
        Ok(())
    }

    fn intr_status(&self) -> Result<IntrStatus> {
        let mut poll_data = vioc_intr_poll::default();
        // Safety: the command copies out one vioc_intr_poll, and that
        // is what `poll_data` is.
        unsafe { self.ioctl(VNA_IOC_INTR_POLL, &mut poll_data) }?;
        Ok(poll_data.vip_status)
    }

    fn ring_intr_clr(&self, ring: u16) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_INTR_CLR, ring as usize)?;
        Ok(())
    }

    /// Wait for `POLLRDBAND`, viona's signal that interrupts are
    /// pending.
    fn wait_intr(&self, stop: &PipeReader) -> Result<IntrWait> {
        loop {
            let mut pfds = [
                libc::pollfd {
                    fd: self.as_raw_fd(),
                    events: INTR_BAND,
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
                return Err(err);
            }
            match poll_verdict(pfds[0].revents, pfds[1].revents) {
                Some(end) => return Ok(end),
                // The wait has no deadline, so poll(2) answers only
                // when something it was asked for is ready, and every
                // one of those has a verdict. A wakeup that lands here
                // is one the loop would meet again at once.
                None => {
                    return Err(std::io::Error::other(
                        "the viona wait woke with no event it asked for",
                    ))
                }
            }
        }
    }

    fn set_notify_iop(&self, rings: &Quiesced<'_>, port: u16) -> Result<()> {
        quiesced_here(rings, self)?;
        self.hook_ioctl_usize(VNA_IOC_SET_NOTIFY_IOP, port as usize)?;
        Ok(())
    }

    fn set_notify_mmio(
        &self,
        rings: &Quiesced<'_>,
        addr: u64,
        size: u32,
    ) -> Result<()> {
        quiesced_here(rings, self)?;
        // A null payload is how the kernel is told to remove the
        // window (`viona_main.c`, `viona_ioc_set_notify_mmio`).
        if addr == 0 {
            self.hook_ioctl_usize(VNA_IOC_SET_NOTIFY_MMIO, 0)?;
            return Ok(());
        }
        let mut vim = vioc_notify_mmio {
            vim_address: addr,
            vim_size: size,
            ..Default::default()
        };
        // Safety: the command copies in one vioc_notify_mmio, and that
        // is what `vim` is.
        unsafe { self.hook_ioctl(VNA_IOC_SET_NOTIFY_MMIO, &mut vim) }?;
        Ok(())
    }

    /// The mode goes by value: this command copies nothing in, and the
    /// kernel casts the data argument straight to its enum
    /// (`viona_main.c`, `viona_ioc_set_promisc`). A pointer here sets
    /// the mode to an address.
    fn set_promisc(&self, mode: PromiscMode) -> Result<()> {
        self.ioctl_usize(VNA_IOC_SET_PROMISC, mode as usize)?;
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        self.ioctl_usize(VNA_IOC_DELETE, 0)?;
        Ok(())
    }
}

/// Refuse a proof made for another link.
fn quiesced_here(rings: &Quiesced<'_>, link: &VionaFd) -> Result<()> {
    if rings.covers(link) {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "the rings of another link were stopped",
    ))
}

/// What one `poll(2)` wakeup means, or `None` to wait again.
///
/// `link` and `stop` are the events each descriptor reported. The stop
/// descriptor is read first and on its own: the halt closes the write
/// end, so it is ready and stays ready, and the poll thread must leave
/// before it touches a link that is about to be destroyed.
///
/// `None` is not "wait again with the same descriptors ready": the
/// caller treats it as an error, because every event the wait asks for
/// is answered above.
fn poll_verdict(link: libc::c_short, stop: libc::c_short) -> Option<IntrWait> {
    if stop != 0 {
        return Some(IntrWait::Stopped);
    }
    if link & INTR_BAND != 0 {
        return Some(IntrWait::Pending);
    }
    // None of these clears on its own, so waiting again would spin.
    if link & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Some(IntrWait::Stopped);
    }
    None
}
