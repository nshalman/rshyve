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
use vmm_api_common::ioctl;

mod ffi;
mod link;

pub use ffi::*;
pub use link::{IntrStatus, IntrWait, LinkOps};

pub const VIONA_DEV_PATH: &str = "/dev/viona";

/// Where a [`VionaFd`] sends the ioctls it makes.
///
/// [`Chan::Dev`] is the link a VM runs on. A test builds the other
/// arm, which keeps the command and the argument and reaches no
/// kernel. Both arms sit under one set of calls: there is a single
/// `delete`, and that is the one a test watches.
///
/// WHY the seam is here and not one layer up: upstream Propolis sends
/// every ioctl straight to `libc::ioctl`
/// (propolis/crates/viona-api/src/lib.rs:164), so a test can read only
/// the errno. A call that names the wrong command, or that swaps two
/// of its arguments, answers with the same errno as the right one.
///
/// A recorder is `#[cfg(test)]`, so no production build can make one.
enum Chan {
    /// An open viona device.
    Dev(File),

    /// A recorder. It holds a file only to answer [`AsRawFd`], and to
    /// give `wait_intr` a descriptor to poll.
    #[cfg(test)]
    Record(File, std::sync::Mutex<Recording>),
}

/// What a recorder keeps, and what it answers with.
#[cfg(test)]
#[derive(Default)]
struct Recording {
    /// The ioctls a caller sent, in order.
    calls: Vec<Call>,

    /// The bytes each command copies back out, by command.
    ///
    /// WHY a recorder needs these: without them a caller reads back
    /// the struct it passed in, so a method whose value comes from
    /// the copy-out answers exactly like one that returns a constant.
    /// `intr_status` and `get_features` are both such methods.
    replies: std::collections::HashMap<i32, Vec<u8>>,
}

/// One ioctl that a caller sent.
#[cfg(test)]
#[derive(Clone, PartialEq, Eq)]
enum Call {
    /// The data argument is a value the kernel reads directly.
    Usize { cmd: i32, arg: usize },

    /// The data argument is a pointer to a struct, kept as the bytes
    /// the kernel would copy in. Without them a call that swaps two
    /// fields of its payload records the same thing as the right one.
    Typed { cmd: i32, data: Vec<u8> },
}

/// The command in hex, the way `uts/intel/sys/viona_io.h` writes it.
///
/// A failed assertion here is read against that header, and a decimal
/// command number is not.
#[cfg(test)]
impl std::fmt::Debug for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Call::Usize { cmd, arg } => {
                write!(f, "Usize {{ cmd: {cmd:#x}, arg: {arg:#x} }}")
            }
            Call::Typed { cmd, data } => {
                write!(f, "Typed {{ cmd: {cmd:#x}, data: {data:02x?} }}")
            }
        }
    }
}

#[cfg(test)]
impl Call {
    /// The command this call sent.
    fn cmd(&self) -> i32 {
        match self {
            Call::Usize { cmd, .. } | Call::Typed { cmd, .. } => *cmd,
        }
    }

    /// The value this call sent the kernel.
    fn value(&self) -> usize {
        match self {
            Call::Usize { arg, .. } => *arg,
            Call::Typed { cmd, .. } => panic!("cmd {cmd:#x} sent no value"),
        }
    }

    /// The payload this call sent the kernel, read back.
    fn payload<T: Copy>(&self) -> T {
        let Call::Typed { cmd, data } = self else {
            panic!("cmd {:#x} sent no payload", self.cmd())
        };
        assert_eq!(
            data.len(),
            size_of::<T>(),
            "cmd {cmd:#x} sent {} bytes",
            data.len(),
        );
        // Safety: the bytes came from one T, and a Vec's buffer has
        // no alignment to promise, so the read is unaligned.
        unsafe { std::ptr::read_unaligned(data.as_ptr().cast::<T>()) }
    }
}

pub struct VionaFd(Chan);
impl VionaFd {
    /// Open viona device and associate it with a given link and vmm instance,
    /// provided in `link_id` and `vm`, respectively.
    pub fn new(link_id: u32, vm: BorrowedFd<'_>) -> Result<Self> {
        let this = Self::open()?;
        this.create(link_id, vm)?;
        Ok(this)
    }

    /// Bind an open handle to a link and a VM.
    ///
    /// Split out of [`Self::new`] because `new` opens the real device
    /// first, so no recorder could reach the call and a test could not
    /// see which field went where.
    fn create(&self, link_id: u32, vm: BorrowedFd<'_>) -> Result<()> {
        let mut vna_create = vioc_create {
            c_linkid: link_id,
            c_vmfd: vm.as_raw_fd(),
        };
        // Safety: the kernel reads one vioc_create for this command.
        let _ = unsafe { self.ioctl(VNA_IOC_CREATE, &mut vna_create) }?;
        Ok(())
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
            // A recorder keeps the bytes going in, which is what says
            // a call filled the payload in the order the kernel reads
            // it. It then writes back what the test armed for this
            // command, so a caller that reads a copy-out reads a
            // value it did not send.
            //
            // Safety: the caller's contract above says `data` points
            // at one initialized T, writable for the same length.
            // Every ioctl payload names its padding, so no byte read
            // here is uninitialized, and the bytes written back came
            // from a value of that size.
            #[cfg(test)]
            Chan::Record(_, rec) => {
                let sent = std::slice::from_raw_parts(
                    data.cast::<u8>(),
                    size_of::<T>(),
                )
                .to_vec();
                let mut rec = rec.lock().expect("the recorded calls");
                rec.calls.push(Call::Typed { cmd, data: sent });
                if let Some(reply) = rec.replies.get(&cmd) {
                    assert_eq!(
                        reply.len(),
                        size_of::<T>(),
                        "cmd {cmd:#x} copies out {} bytes",
                        size_of::<T>(),
                    );
                    std::ptr::copy_nonoverlapping(
                        reply.as_ptr(),
                        data.cast::<u8>(),
                        reply.len(),
                    );
                }
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
            Chan::Record(_, rec) => {
                rec.lock()
                    .expect("the recorded calls")
                    .calls
                    .push(Call::Usize { cmd, arg: data });
                Ok(1)
            }
        }
    }

    /// Read the features the kernel driver offers.
    pub fn get_features(&self) -> Result<u64> {
        let mut feat: u64 = 0;
        // Safety: the command copies out one u64, and that is what
        // `feat` is.
        unsafe { self.ioctl(VNA_IOC_GET_FEATURES, &mut feat) }?;
        Ok(feat)
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
