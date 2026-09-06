// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copied from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream: crates/viona-api/src/lib.rs
// https://github.com/oxidecomputer/propolis

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::os::fd::*;
use std::os::unix::fs::MetadataExt;

mod ffi;

pub use ffi::*;

pub const VIONA_DEV_PATH: &str = "/dev/viona";

/// Where a [`VionaFd`] sends the ioctls it makes.
///
/// [`Chan::Dev`] is the link a VM runs on. A test builds the other
/// arm, which keeps the command and the argument and reaches no
/// kernel. Both arms sit under one set of wrappers: there is a single
/// [`VionaFd::delete`], and that is the one a test watches.
///
/// WHY the seam is here and not in the wrappers: upstream Propolis
/// sends every ioctl straight to `libc::ioctl`
/// (propolis/crates/viona-api/src/lib.rs:164), so a test can read only
/// the errno. A wrapper that names the wrong command, or that drops
/// its ring argument, answers with the same errno as the right one.
enum Chan {
    /// An open viona device.
    Dev(File),

    /// A recorder. It holds a file only to answer [`AsRawFd`].
    #[cfg(test)]
    Record(File, std::sync::Mutex<Vec<Call>>),
}

/// One ioctl that a wrapper sent.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    /// The data argument is a value the kernel reads directly.
    Usize { cmd: i32, arg: usize },

    /// The data argument is a pointer to a struct. A recorder copies
    /// nothing, so only the command is kept.
    Typed { cmd: i32 },
}

pub struct VionaFd(Chan);
impl VionaFd {
    /// Open viona device and associate it with a given link and vmm instance,
    /// provided in `link_id` and `vm_fd`, respectively.
    pub fn new(link_id: u32, vm_fd: RawFd) -> Result<Self> {
        let this = Self::open()?;

        let mut vna_create = vioc_create {
            c_linkid: link_id,
            c_vmfd: vm_fd,
        };
        let _ = unsafe { this.ioctl(VNA_IOC_CREATE, &mut vna_create) }?;
        Ok(this)
    }

    /// Open viona device instance without performing any other initialization
    pub fn open() -> Result<Self> {
        let fp = OpenOptions::new()
            .read(true)
            .write(true)
            .open(VIONA_DEV_PATH)?;

        Ok(Self(Chan::Dev(fp)))
    }

    /// The open file this handle answers for.
    fn file(&self) -> &File {
        match &self.0 {
            Chan::Dev(fp) => fp,
            #[cfg(test)]
            Chan::Record(fp, _) => fp,
        }
    }

    /// Issue ioctl against open viona instance
    ///
    /// # Safety
    ///
    /// Caller is charged with providing `data` argument which is adequate for
    /// any copyin/copyout actions which may occur as part of the ioctl
    /// processing.
    pub unsafe fn ioctl<T>(&self, cmd: i32, data: *mut T) -> Result<i32> {
        match &self.0 {
            Chan::Dev(fp) => {
                ioctl(fp.as_raw_fd(), cmd, data as *mut libc::c_void)
            }
            // A recorder keeps the command and copies nothing, so a
            // caller reads back the struct it passed in. That is
            // enough to say which entry point a wrapper reached.
            #[cfg(test)]
            Chan::Record(_, calls) => {
                calls
                    .lock()
                    .expect("the recorded calls")
                    .push(Call::Typed { cmd });
                Ok(0)
            }
        }
    }

    pub fn ioctl_usize(&self, cmd: i32, data: usize) -> Result<i32> {
        if !Self::ioctl_usize_safe(cmd) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "unsafe cmd provided",
            ));
        }
        match &self.0 {
            // Safety: Since we are explicitly filtering for vmm ioctls
            // which will not assume the data argument is a pointer for
            // copyin/copyout, we can dismiss those dangers.  The caller
            // is assumed to be cognizant of other potential side
            // effects.
            Chan::Dev(fp) => unsafe {
                ioctl(fp.as_raw_fd(), cmd, data as *mut libc::c_void)
            },
            // A recorder answers 1, which is a valid version, so
            // `api_version` does not trip its own demand.
            #[cfg(test)]
            Chan::Record(_, calls) => {
                calls
                    .lock()
                    .expect("the recorded calls")
                    .push(Call::Usize { cmd, arg: data });
                Ok(1)
            }
        }
    }

    /// Destroy the in-kernel link, which gives back its `vmm_drv` hold.
    ///
    /// WHY teardown must issue this: viona holds the VM for the life of
    /// the link (`viona_main.c`, `vmm_drv_hold`). `VM_DESTROY_SELF`
    /// purges the holds and then waits for every lease to break in an
    /// untimed `cv_wait` (`vmm_sol_dev.c`, `vmm_drv_purge`). Releasing
    /// the last hold first clears `VMM_HELD`, and the purge skips that
    /// wait.
    ///
    /// The call itself waits for each ring worker to stop, in a
    /// `cv_wait` that does not heed signals (`viona_ring.c`,
    /// `viona_ring_reset`). Run it only where a lost thread is
    /// acceptable.
    pub fn delete(&self) -> Result<()> {
        self.ioctl_usize(VNA_IOC_DELETE, 0)?;
        Ok(())
    }

    /// Return one ring to its reset state.
    ///
    /// [`Self::delete`] resets every ring anyway, in a wait that
    /// ignores signals. A ring that is reset already leaves that wait
    /// at once, so each reset here is one wait the delete does not
    /// make. This wait heeds signals and can return `EINTR`.
    pub fn ring_reset(&self, ring: usize) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_RESET, ring)?;
        Ok(())
    }

    /// Tell the kernel worker to look at one ring again.
    ///
    /// This is the guest kick. A ring that was never programmed has no
    /// worker, and the kernel refuses the call.
    pub fn ring_kick(&self, ring: usize) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_KICK, ring)?;
        Ok(())
    }

    /// Stop one ring's worker and keep its state.
    ///
    /// A migration source pauses each ring before it reads the ring
    /// indices out, so the kernel does not move them again.
    pub fn ring_pause(&self, ring: usize) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_PAUSE, ring)?;
        Ok(())
    }

    /// Clear one ring's pending interrupt.
    ///
    /// The poll thread does this before it raises the interrupt to the
    /// guest. Until it runs, the kernel signals the same interrupt
    /// again and the poll thread spins.
    pub fn ring_intr_clr(&self, ring: usize) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_INTR_CLR, ring)?;
        Ok(())
    }

    /// Name the I/O port the guest writes to kick a ring.
    ///
    /// The kernel then takes the kick itself, and the vCPU does not
    /// leave to userspace for it.
    pub fn set_notify_iop(&self, port: u16) -> Result<()> {
        self.ioctl_usize(VNA_IOC_SET_NOTIFY_IOP, port as usize)?;
        Ok(())
    }

    /// Set what traffic the link passes to the guest.
    ///
    /// The mode goes by value: this command copies nothing in, and the
    /// kernel casts the data argument straight to its enum
    /// (`viona_main.c`, `viona_ioc_set_promisc`). A pointer here sets
    /// the mode to an address.
    pub fn set_promisc(&self, mode: PromiscMode) -> Result<()> {
        self.ioctl_usize(VNA_IOC_SET_PROMISC, mode as usize)?;
        Ok(())
    }

    /// Program one ring's addresses, in the modern (VirtIO 1.0) layout.
    ///
    /// The kernel starts a worker for the ring, and resets the ring
    /// indices to 0. A migration destination needs
    /// [`Self::ring_set_state`] instead.
    pub fn ring_init_modern(
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

    /// Program one ring's addresses and its indices together.
    ///
    /// A migration destination resumes the kernel where the source
    /// stopped, which [`Self::ring_init_modern`] cannot do.
    pub fn ring_set_state(&self, state: &vioc_ring_state) -> Result<()> {
        let mut state = *state;
        // Safety: the command copies in one vioc_ring_state, and that
        // is what `state` is.
        unsafe { self.ioctl(VNA_IOC_RING_SET_STATE, &mut state) }?;
        Ok(())
    }

    /// Read one ring's addresses and indices.
    ///
    /// A migration source reads the indices out after it pauses the
    /// ring, so the kernel does not move them again.
    pub fn ring_get_state(&self, ring: u16) -> Result<vioc_ring_state> {
        let mut state = vioc_ring_state {
            vrs_index: ring,
            ..Default::default()
        };
        // Safety: the command copies out one vioc_ring_state, and that
        // is what `state` is.
        unsafe { self.ioctl(VNA_IOC_RING_GET_STATE, &mut state) }?;
        Ok(state)
    }

    /// Route one ring's interrupt to an MSI-X message.
    ///
    /// The kernel then writes the message itself, and the userspace
    /// poll thread has nothing to raise for that ring.
    pub fn ring_set_msi(&self, ring: u16, addr: u64, msg: u64) -> Result<()> {
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

    /// Read which rings have an interrupt pending.
    ///
    /// One entry per ring, non-zero for a ring the kernel has work
    /// waiting on.
    pub fn intr_status(&self) -> Result<[u32; VIONA_VQ_MAX as usize]> {
        let mut poll_data = vioc_intr_poll::default();
        // Safety: the command copies out one vioc_intr_poll, and that
        // is what `poll_data` is.
        unsafe { self.ioctl(VNA_IOC_INTR_POLL, &mut poll_data) }?;
        Ok(poll_data.vip_status)
    }

    /// Give the kernel the features the guest accepted.
    ///
    /// `VIRTIO_F_VERSION_1` is the one that puts the kernel rings in
    /// the modern layout.
    pub fn set_features(&self, features: u64) -> Result<()> {
        let mut feat = features;
        // Safety: the command copies in one u64, and that is what
        // `feat` is.
        unsafe { self.ioctl(VNA_IOC_SET_FEATURES, &mut feat) }?;
        Ok(())
    }

    /// Read the features the kernel driver offers.
    pub fn get_features(&self) -> Result<u64> {
        let mut feat: u64 = 0;
        // Safety: the command copies out one u64, and that is what
        // `feat` is.
        unsafe { self.ioctl(VNA_IOC_GET_FEATURES, &mut feat) }?;
        Ok(feat)
    }

    /// Name the memory address the guest writes to kick a ring.
    ///
    /// The modern transport counterpart of [`Self::set_notify_iop`].
    pub fn set_notify_mmio(&self, addr: u64, size: u32) -> Result<()> {
        let mut vim = vioc_notify_mmio {
            vim_address: addr,
            vim_size: size,
        };
        // Safety: the command copies in one vioc_notify_mmio, and that
        // is what `vim` is.
        unsafe { self.ioctl(VNA_IOC_SET_NOTIFY_MMIO, &mut vim) }?;
        Ok(())
    }

    /// Query the API version exposed by the kernel VMM.
    pub fn api_version(&self) -> Result<u32> {
        let vers = self.ioctl_usize(VNA_IOC_VERSION, 0)?;

        // We expect and demand a positive version number from the
        // VNA_IOC_VERSION interface.
        assert!(vers > 0);
        Ok(vers as u32)
    }

    /// Retrieve the minor number of the viona device instance.
    /// This is used for matching kernel statistic entries to the viona device.
    pub fn instance_id(&self) -> Result<u32> {
        let meta = self.file().metadata()?;
        Ok(minor(&meta))
    }

    /// Check viona ioctl command against those known to not require any
    /// copyin/copyout to function.
    const fn ioctl_usize_safe(cmd: i32) -> bool {
        matches!(
            cmd,
            VNA_IOC_DELETE
                | VNA_IOC_RING_RESET
                | VNA_IOC_RING_KICK
                | VNA_IOC_RING_PAUSE
                | VNA_IOC_RING_INTR_CLR
                | VNA_IOC_VERSION
                | VNA_IOC_SET_NOTIFY_IOP
                | VNA_IOC_SET_PROMISC
                | VNA_IOC_GET_MTU
                | VNA_IOC_SET_MTU
                | VNA_IOC_GET_PAIRS
                | VNA_IOC_SET_PAIRS
                | VNA_IOC_GET_USEPAIRS
                | VNA_IOC_SET_USEPAIRS,
        )
    }
}
impl AsRawFd for VionaFd {
    fn as_raw_fd(&self) -> RawFd {
        self.file().as_raw_fd()
    }
}

#[cfg(target_os = "illumos")]
unsafe fn ioctl(fd: RawFd, cmd: i32, data: *mut libc::c_void) -> Result<i32> {
    match libc::ioctl(fd, cmd, data) {
        -1 => Err(Error::last_os_error()),
        other => Ok(other),
    }
}

#[cfg(not(target_os = "illumos"))]
unsafe fn ioctl(
    _fd: RawFd,
    _cmd: i32,
    _data: *mut libc::c_void,
) -> Result<i32> {
    Err(Error::other("illumos required"))
}

#[cfg(target_os = "illumos")]
fn minor(meta: &std::fs::Metadata) -> u32 {
    // With #4208 backported into libc-0.2, minor() became a const-fn for
    // practically all of the UNIX-y platforms, save for illumos.
    //
    // Until we address that, just paper over it with a wrapper here.
    // Viona is not usable anywhere but illumos.
    unsafe { libc::minor(meta.rdev()) }
}
#[cfg(not(target_os = "illumos"))]
fn minor(meta: &std::fs::Metadata) -> u32 {
    let _rdev = meta.rdev();
    panic!("illumos required");
}

/// Convenience constants to provide some documentation on what changes have
/// been introduced in the various viona API versions.
#[repr(u32)]
#[derive(Copy, Clone)]
pub enum ApiVersion {
    /// Adds multi-queue support and change the data structure for per-queue
    /// interrupt polling to a compact bitmap.
    V6 = 6,

    /// Adds support for VirtIO 1.0 (modern) virtqueues.
    V5 = 5,

    /// Adds support for getting/setting MTU
    V4 = 4,

    /// Adds support for interface parameters
    V3 = 3,

    /// Adds support for non-vnic datalink devices
    V2 = 2,

    /// Initial version available for query
    V1 = 1,
}
impl ApiVersion {
    pub const fn current() -> Self {
        Self::V6
    }
}
impl PartialEq<ApiVersion> for u32 {
    fn eq(&self, other: &ApiVersion) -> bool {
        *self == *other as u32
    }
}
impl PartialOrd<ApiVersion> for u32 {
    fn partial_cmp(&self, other: &ApiVersion) -> Option<std::cmp::Ordering> {
        Some(self.cmp(&(*other as u32)))
    }
}

use vmm_api_common::CachedVersion;

static VERSION_CACHE: CachedVersion = CachedVersion::new();

/// Query the API version from the viona device on the system.
///
/// Caches said version (or any emitted error) for later calls. The API version
/// may be used at runtime in operating the virtual machine, where the delay to
/// query again would be more directly guest-impactful.
pub fn api_version() -> Result<u32> {
    VERSION_CACHE.get_or_init(|| -> Result<u32> {
        let ctl = VionaFd::open()?;
        let vers = ctl.api_version()?;
        Ok(vers)
    })
}

#[cfg(test)]
mod test;
