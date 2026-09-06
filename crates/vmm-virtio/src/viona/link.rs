// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The in-kernel calls the shared device logic makes.
//!
//! Behind a trait for two reasons: off illumos there is no link to
//! call, and a test can read what the device asked of the kernel
//! without one. The illumos build calls the same trait through the
//! same guards, so a test off illumos pins the code the VM runs.
//!
//! The calls that only a migration or a boot makes stay on
//! [`viona_api::VionaFd`], in [`super::migrate`] and [`super::setup`].

use std::io::Result;

/// The kernel side of the calls that run on every platform.
pub(super) trait LinkOps: Send + Sync {
    /// Program one ring's addresses, in the modern layout.
    ///
    /// The kernel starts a worker for the ring, so nothing may call
    /// this for a link the halt destroyed.
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

    /// Give the kernel the features the guest accepted.
    fn set_features(&self, features: u64) -> Result<()>;

    /// Destroy the link, which gives back its `vmm_drv` hold.
    fn delete(&self) -> Result<()>;
}

#[cfg(target_os = "illumos")]
impl LinkOps for viona_api::VionaFd {
    fn ring_init(
        &self,
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> Result<()> {
        let mut rim = viona_api::vioc_ring_init_modern {
            rim_index: ring,
            rim_qsize: size,
            rim_qaddr_desc: desc,
            rim_qaddr_avail: avail,
            rim_qaddr_used: used,
            ..Default::default()
        };
        // Safety: the command copies in one vioc_ring_init_modern, and
        // that is what `rim` is.
        unsafe { self.ioctl(viona_api::VNA_IOC_RING_INIT_MODERN, &mut rim) }?;
        Ok(())
    }

    fn ring_kick(&self, ring: u16) -> Result<()> {
        viona_api::VionaFd::ring_kick(self, ring as usize)
    }

    fn ring_reset(&self, ring: u16) -> Result<()> {
        viona_api::VionaFd::ring_reset(self, ring as usize)
    }

    fn set_features(&self, features: u64) -> Result<()> {
        let mut feat = features;
        // Safety: the command copies in one u64, and that is what
        // `feat` is.
        unsafe { self.ioctl(viona_api::VNA_IOC_SET_FEATURES, &mut feat) }?;
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        viona_api::VionaFd::delete(self)
    }
}
