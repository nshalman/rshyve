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

pub struct VionaFd(File);
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

        Ok(Self(fp))
    }

    /// Issue ioctl against open viona instance
    ///
    /// # Safety
    ///
    /// Caller is charged with providing `data` argument which is adequate for
    /// any copyin/copyout actions which may occur as part of the ioctl
    /// processing.
    pub unsafe fn ioctl<T>(&self, cmd: i32, data: *mut T) -> Result<i32> {
        ioctl(self.as_raw_fd(), cmd, data as *mut libc::c_void)
    }

    pub fn ioctl_usize(&self, cmd: i32, data: usize) -> Result<i32> {
        if !Self::ioctl_usize_safe(cmd) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "unsafe cmd provided",
            ));
        }
        // Safety: Since we are explicitly filtering for vmm ioctls which will
        // not assume the data argument is a pointer for copyin/copyout, we can
        // dismiss those dangers.  The caller is assumed to be cognizant of
        // other potential side effects.
        unsafe { ioctl(self.as_raw_fd(), cmd, data as *mut libc::c_void) }
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
        let meta = self.0.metadata()?;
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
        self.0.as_raw_fd()
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
mod test {
    use super::*;

    /// An open fd that is not a viona link.
    ///
    /// The ioctls below reach the kernel and are refused there, so the
    /// wrappers are testable off illumos. /dev/null answers every
    /// ioctl with ENXIO (`uts/common/io/mem.c`, `mmioctl`).
    fn not_a_link() -> VionaFd {
        VionaFd(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .expect("every unix has /dev/null"),
        )
    }

    #[test]
    fn latest_api_version() {
        let cur = ApiVersion::current();
        assert_eq!(VIONA_CURRENT_INTERFACE_VERSION, cur as u32);
    }

    #[test]
    fn u32_comparisons() {
        assert!(1u32 < ApiVersion::V2);
        assert!(2u32 == ApiVersion::V2);
        assert!(3u32 > ApiVersion::V2);
    }

    #[test]
    fn the_halt_commands_match_the_kernel() {
        // uts/intel/sys/viona_io.h. A number that drifts sends the
        // halt to some other viona entry point, and the vmm_drv hold
        // it must give back stays held.
        assert_eq!(VNA_IOC_DELETE, 0x0056_4302);
        assert_eq!(VNA_IOC_RING_RESET, 0x0056_4311);
    }

    #[test]
    fn the_halt_commands_may_take_a_usize() {
        // delete() and ring_reset() go through ioctl_usize. A cmd the
        // allowlist misses is refused before the kernel sees it, and
        // the halt would then leave viona's hold on the VM.
        assert!(VionaFd::ioctl_usize_safe(VNA_IOC_DELETE));
        assert!(VionaFd::ioctl_usize_safe(VNA_IOC_RING_RESET));
    }

    #[test]
    fn no_command_that_copies_in_may_take_a_usize() {
        // ioctl_usize hands its argument to the kernel as the data
        // pointer. A cmd that copies in would read the usize as a
        // user address of the caller's choice.
        for cmd in [
            VNA_IOC_CREATE,
            VNA_IOC_DEFAULT_PARAMS,
            VNA_IOC_RING_INIT,
            VNA_IOC_RING_INIT_MODERN,
            VNA_IOC_RING_SET_MSI,
            VNA_IOC_RING_SET_STATE,
            VNA_IOC_RING_GET_STATE,
            VNA_IOC_INTR_POLL,
            VNA_IOC_INTR_POLL_MQ,
            VNA_IOC_SET_FEATURES,
            VNA_IOC_GET_FEATURES,
            VNA_IOC_SET_NOTIFY_MMIO,
            VNA_IOC_GET_PARAMS,
            VNA_IOC_SET_PARAMS,
        ] {
            assert!(
                !VionaFd::ioctl_usize_safe(cmd),
                "cmd {cmd:#x} copies in through its data argument",
            );
        }
    }

    #[test]
    fn a_command_that_copies_in_is_refused_at_the_call() {
        // The allowlist has to hold at the call, not only in the
        // const fn that lists it.
        let err = not_a_link()
            .ioctl_usize(VNA_IOC_RING_SET_STATE, 0x4141_4141)
            .expect_err("a copyin cmd may not take a usize");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn a_failed_delete_reports_it() {
        // The halt calls this and carries on to VM_DESTROY_SELF. A
        // failure it swallowed would hide the destroy wedge that
        // follows.
        let err = not_a_link().delete().expect_err("/dev/null holds no link");
        assert_ne!(
            err.kind(),
            ErrorKind::InvalidInput,
            "the delete never reached the ioctl",
        );
    }

    #[test]
    fn a_failed_ring_reset_reports_it() {
        // The halt resets every ring before the delete and reports
        // what fails. An out of range ring is the kernel's to refuse.
        let fd = not_a_link();
        for ring in [0, 1, usize::MAX] {
            let err = fd.ring_reset(ring).expect_err("/dev/null holds no link");
            assert_ne!(
                err.kind(),
                ErrorKind::InvalidInput,
                "ring {ring} never reached the ioctl",
            );
        }
    }
}
