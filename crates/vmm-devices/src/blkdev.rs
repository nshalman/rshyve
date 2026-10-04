// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Durable flush and write-cache control for a disk backing fd.
//!
//! Zvols default to write-through on illumos; every sync 4K write commits
//! the ZIL. Writeback with guest-driven flushes is the difference between
//! ~1k and ~50k+ 4K randwrite IOPS. Model lifted from Oxide's propolis
//! (`lib/propolis/src/block/file.rs`): probe at open, enable writeback,
//! turn it back off on Drop.
//!
//! A disk that cannot flush is accepted only with its write cache
//! confirmed off, or with no cache control at all. The write cache is
//! device-global and other opens of the device may be attached, so this
//! code only ever moves it toward off after open.

use std::fs::File;
use std::io;
#[cfg(target_os = "illumos")]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::Arc;

#[cfg(target_os = "illumos")]
const DKIOC: libc::c_int = 0x04 << 8; // 0x0400
#[cfg(target_os = "illumos")]
const DKIOCGETWCE: libc::c_int = DKIOC | 36; // 0x0424
#[cfg(target_os = "illumos")]
const DKIOCSETWCE: libc::c_int = DKIOC | 37; // 0x0425
#[cfg(target_os = "illumos")]
const DKIOCFLUSHWRITECACHE: libc::c_int = DKIOC | 34; // 0x0422

/// The disk ioctls the flush policy rests on, so tests can inject results.
#[cfg(any(target_os = "illumos", test))]
pub(crate) trait DiskOps: Send + Sync {
    fn flush(&self) -> io::Result<()>;
    fn get_wce(&self) -> io::Result<bool>;
    fn set_wce(&self, on: bool) -> io::Result<()>;
}

/// Makes completed writes on a backing fd durable.
///
/// How is decided once, at open, together with the write cache state.
/// A character disk device takes DKIOCFLUSHWRITECACHE rather than
/// fsync(2): illumos fsync skips the flush when no I/O marked the device
/// since the last one, so two flushes that race a write can both miss
/// it, and it ignores the result of the flush it does send.
pub struct DiskSync {
    file: Arc<File>,
    mode: Mode,
}

enum Mode {
    ReadOnly,
    /// A regular file or a block device. On illumos `sync_data` is
    /// fsync, and fsync of a ZFS file commits the ZIL. Specfs answers
    /// ENOTTY to every ioctl on a block node, and writes to it wait in
    /// the specfs page cache that only fsync pushes out. spec_fsync
    /// drops the flush errno, so a failed flush on a block node cannot
    /// reach us; that is why disk paths should name the rdsk node.
    Fsync,
    #[cfg(any(target_os = "illumos", test))]
    Device(Device),
}

impl DiskSync {
    /// Classify `file`, probe its flush and set its write cache.
    ///
    /// Only a writable character device is probed. It is refused when:
    /// - the probe flush fails with an errno other than ENOTSUP or ENOTTY;
    /// - it cannot flush and its write cache cannot be confirmed off;
    /// - a flush fails with any other errno after the write cache was
    ///   turned on.
    pub fn new(file: Arc<File>, read_only: bool) -> io::Result<Self> {
        #[cfg(target_os = "illumos")]
        {
            use std::os::unix::fs::MetadataExt;

            let mode = file.metadata()?.mode();
            let ops = Arc::new(Dkioc(Arc::clone(&file)));
            Self::build(file, mode, read_only, ops)
        }
        #[cfg(not(target_os = "illumos"))]
        {
            let mode = if read_only {
                Mode::ReadOnly
            } else {
                Mode::Fsync
            };
            Ok(Self { file, mode })
        }
    }

    #[cfg(any(target_os = "illumos", test))]
    fn build(
        file: Arc<File>,
        st_mode: u32,
        read_only: bool,
        ops: Arc<dyn DiskOps>,
    ) -> io::Result<Self> {
        let mode = if read_only {
            Mode::ReadOnly
        } else if st_mode & libc::S_IFMT == libc::S_IFCHR {
            Mode::Device(Device::open(ops)?)
        } else {
            Mode::Fsync
        };
        Ok(Self { file, mode })
    }

    pub fn file(&self) -> &Arc<File> {
        &self.file
    }

    /// Make every write that has completed on the fd durable.
    pub fn sync(&self) -> io::Result<()> {
        match &self.mode {
            Mode::ReadOnly => Ok(()),
            Mode::Fsync => self.file.sync_data(),
            #[cfg(any(target_os = "illumos", test))]
            Mode::Device(dev) => dev.sync(),
        }
    }
}

#[cfg(any(target_os = "illumos", test))]
struct Device {
    ops: Arc<dyn DiskOps>,
    can_flush: bool,
    /// This open turned the write cache on; Drop turns it off.
    enabled_wce: bool,
}

#[cfg(any(target_os = "illumos", test))]
impl Device {
    fn open(ops: Arc<dyn DiskOps>) -> io::Result<Self> {
        let mut dev = Self {
            ops,
            can_flush: true,
            enabled_wce: false,
        };
        match dev.ops.flush() {
            Ok(()) => {}
            Err(e) if cannot_flush(&e) => {
                dev.cache_off_for_good()?;
                return Ok(dev);
            }
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("disk write cache flush failed at open: {e}"),
                ))
            }
        }
        // Best effort: a disk that flushes is safe either way.
        if !matches!(dev.ops.get_wce(), Ok(false))
            || dev.ops.set_wce(true).is_err()
        {
            return Ok(dev);
        }
        dev.enabled_wce = true;
        // sd answers the flush with success and sends nothing while the
        // cache is off, so only this probe tests SYNCHRONIZE CACHE.
        match dev.ops.flush() {
            Ok(()) => Ok(dev),
            Err(e) if cannot_flush(&e) => {
                dev.cache_off_for_good()?;
                dev.enabled_wce = false;
                Ok(dev)
            }
            // Drop turns the cache back off.
            Err(e) => Err(io::Error::new(
                e.kind(),
                format!("disk flush failed after write cache enable: {e}"),
            )),
        }
    }

    /// Turn the write cache off and confirm it, for good: turning it
    /// back on later could expose another open of this device that
    /// also cannot flush.
    fn cache_off_for_good(&mut self) -> io::Result<()> {
        self.can_flush = false;
        match self.ops.get_wce() {
            Ok(false) => Ok(()),
            // No write cache control: the nvme blkdev namespace without
            // a volatile cache answers this way.
            Err(e) if cannot_flush(&e) => Ok(()),
            Err(e) => Err(refuse(e)),
            Ok(true) => {
                self.ops.set_wce(false).map_err(refuse)?;
                if self.ops.get_wce().map_err(refuse)? {
                    return Err(refuse(io::Error::other(
                        "write cache still on after DKIOCSETWCE=0",
                    )));
                }
                Ok(())
            }
        }
    }

    fn sync(&self) -> io::Result<()> {
        if self.can_flush {
            self.ops.flush()
        } else {
            Ok(())
        }
    }
}

#[cfg(any(target_os = "illumos", test))]
impl Drop for Device {
    fn drop(&mut self) {
        if self.enabled_wce {
            let _ = self.ops.set_wce(false);
        }
    }
}

/// ENOTSUP says SYNCHRONIZE CACHE is unsupported, not that no cache is
/// on; ENOTTY only says the ioctl is unknown. Neither proves the write
/// cache is off.
#[cfg(any(target_os = "illumos", test))]
fn cannot_flush(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::ENOTTY))
}

#[cfg(any(target_os = "illumos", test))]
fn refuse(e: io::Error) -> io::Error {
    io::Error::new(
        e.kind(),
        format!(
            "disk cannot flush and its write cache could not be \
             confirmed off: {e}"
        ),
    )
}

/// The ioctls on a real fd.
#[cfg(target_os = "illumos")]
struct Dkioc(Arc<File>);

#[cfg(target_os = "illumos")]
impl DiskOps for Dkioc {
    fn flush(&self) -> io::Result<()> {
        // SAFETY: a null argument asks for a synchronous flush; the
        // kernel reads nothing through it.
        unsafe {
            vmm_api_common::ioctl(
                self.0.as_raw_fd(),
                DKIOCFLUSHWRITECACHE,
                std::ptr::null_mut(),
            )
        }?;
        Ok(())
    }

    fn get_wce(&self) -> io::Result<bool> {
        let mut v: libc::c_int = 0;
        dkioc_wce(self.0.as_fd(), DKIOCGETWCE, &mut v)?;
        Ok(v != 0)
    }

    fn set_wce(&self, on: bool) -> io::Result<()> {
        let mut v = libc::c_int::from(on);
        dkioc_wce(self.0.as_fd(), DKIOCSETWCE, &mut v)
    }
}

/// Typed wrapper around the variadic `libc::ioctl`.
///
/// Passes `&mut c_int` explicitly so the integer doesn't hit variadic
/// type promotion and the kernel reads/writes the expected 4 bytes.
#[cfg(target_os = "illumos")]
fn dkioc_wce(
    fd: BorrowedFd<'_>,
    cmd: libc::c_int,
    v: &mut libc::c_int,
) -> io::Result<()> {
    // SAFETY: `v` lives through the call and is the size
    // DKIOC{GET,SET}WCE reads and writes.
    unsafe {
        vmm_api_common::ioctl(fd.as_raw_fd(), cmd, std::ptr::from_mut(v).cast())
    }?;
    Ok(())
}

#[cfg(test)]
mod tests;
