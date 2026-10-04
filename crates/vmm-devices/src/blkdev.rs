// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Zvol write-cache control (DKIOCSETWCE) and durable flush.
//!
//! Zvols default to write-through on illumos; every sync 4K write commits
//! the ZIL. Flipping the zvol to writeback (with guest-driven flushes for
//! durability) is the difference between ~1k and ~50k+ 4K randwrite IOPS.
//!
//! Model lifted from Oxide's propolis (`lib/propolis/src/block/file.rs`):
//! probe at open, enable writeback if supported, restore the device's
//! original state on Drop. bhyve's feature-gated toggle is not needed —
//! we always advertise `VIRTIO_BLK_F_FLUSH` and every real guest negotiates
//! it, and NVMe has implicit FLUSH semantics.
//!
//! `DiskCache` shares the `File`, so the restore ioctl always runs on
//! an open descriptor whatever the field order of the owner.

use std::fs::File;
use std::io;
#[cfg(target_os = "illumos")]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
#[cfg(target_os = "illumos")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(target_os = "illumos")]
const DKIOC: libc::c_int = 0x04 << 8; // 0x0400
#[cfg(target_os = "illumos")]
const DKIOCGETWCE: libc::c_int = DKIOC | 36; // 0x0424
#[cfg(target_os = "illumos")]
const DKIOCSETWCE: libc::c_int = DKIOC | 37; // 0x0425
#[cfg(target_os = "illumos")]
const DKIOCFLUSHWRITECACHE: libc::c_int = DKIOC | 34; // 0x0422

/// Holds zvol write-cache state for the lifetime of a backing fd.
///
/// `initial == Some(false)` means we enabled writeback and must restore
/// on Drop. `Some(true)` means the device was already in writeback.
/// `None` means we never touched it (read-only, non-illumos, or a device
/// that didn't support DKIOCSETWCE).
pub struct DiskCache {
    #[cfg_attr(not(target_os = "illumos"), allow(dead_code))]
    file: Arc<File>,
    #[cfg_attr(not(target_os = "illumos"), allow(dead_code))]
    initial: Option<bool>,
}

impl DiskCache {
    /// Probe and (where supported) enable writeback on the backing file.
    ///
    /// Best-effort: any ioctl failure leaves the device state unchanged.
    pub fn new(file: Arc<File>, read_only: bool) -> Self {
        if read_only {
            return Self {
                file,
                initial: None,
            };
        }
        Self::new_inner(file)
    }

    #[cfg(target_os = "illumos")]
    fn new_inner(file: Arc<File>) -> Self {
        let mut v: libc::c_int = 0;
        if dkioc_wce(file.as_fd(), DKIOCGETWCE, &mut v).is_err() {
            // Not a disk device, or the backing store doesn't expose WCE
            // (regular files on illumos, iSCSI/COMSTAR targets, etc.).
            return Self {
                file,
                initial: None,
            };
        }
        let initial = v != 0;
        if initial {
            // Already in writeback; nothing to do, nothing to restore.
            return Self {
                file,
                initial: None,
            };
        }
        let mut enable: libc::c_int = 1;
        if dkioc_wce(file.as_fd(), DKIOCSETWCE, &mut enable).is_err() {
            // GET worked but SET refused — leave state alone.
            return Self {
                file,
                initial: None,
            };
        }
        Self {
            file,
            initial: Some(false),
        }
    }

    #[cfg(not(target_os = "illumos"))]
    fn new_inner(file: Arc<File>) -> Self {
        Self {
            file,
            initial: None,
        }
    }
}

impl Drop for DiskCache {
    fn drop(&mut self) {
        #[cfg(target_os = "illumos")]
        {
            if self.initial == Some(false) {
                let mut v: libc::c_int = 0;
                let _ = dkioc_wce(self.file.as_fd(), DKIOCSETWCE, &mut v);
            }
        }
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

/// Makes completed writes on a backing fd durable.
///
/// The fd is classified once at open. A character device takes
/// DKIOCFLUSHWRITECACHE rather than fsync(2): illumos fsync skips the
/// flush when no I/O marked the device since the last one, so two
/// flushes that race a write can both miss it, and it ignores the
/// result of the flush it does send.
pub struct DiskSync {
    file: Arc<File>,
    #[cfg(target_os = "illumos")]
    method: Method,
}

#[cfg(target_os = "illumos")]
enum Method {
    SyncData,
    FlushCache {
        /// Set once the driver reports it has no write cache.
        no_cache: AtomicBool,
    },
}

impl DiskSync {
    pub fn new(file: Arc<File>) -> io::Result<Self> {
        #[cfg(target_os = "illumos")]
        {
            use std::os::unix::fs::FileTypeExt;

            let method = if file.metadata()?.file_type().is_char_device() {
                Method::FlushCache {
                    no_cache: AtomicBool::new(false),
                }
            } else {
                Method::SyncData
            };
            Ok(Self { file, method })
        }
        #[cfg(not(target_os = "illumos"))]
        Ok(Self { file })
    }

    pub fn file(&self) -> &Arc<File> {
        &self.file
    }

    /// Make every write that has completed on the fd durable.
    pub fn sync(&self) -> io::Result<()> {
        #[cfg(target_os = "illumos")]
        if let Method::FlushCache { no_cache } = &self.method {
            if no_cache.load(Ordering::Relaxed) {
                return Ok(());
            }
            // SAFETY: a null argument asks for a synchronous flush; the
            // kernel reads nothing through it.
            let res = unsafe {
                vmm_api_common::ioctl(
                    self.file.as_raw_fd(),
                    DKIOCFLUSHWRITECACHE,
                    std::ptr::null_mut(),
                )
            };
            return match res {
                Ok(_) => Ok(()),
                Err(e) if means_no_cache(&e) => {
                    no_cache.store(true, Ordering::Relaxed);
                    Ok(())
                }
                Err(e) => Err(e),
            };
        }
        self.file.sync_data()
    }
}

/// Only these say the driver has no cache to flush. ENXIO and the
/// rest can mean the device or its media is gone, so success there
/// would lie to the guest.
#[cfg(any(target_os = "illumos", test))]
fn means_no_cache(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::ENOTTY))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_regular_file_syncs() {
        let mut file = tempfile::tempfile().expect("tempfile");
        file.write_all(&[0x5A; 512]).expect("write");
        let disk = DiskSync::new(Arc::new(file)).expect("classify");
        disk.sync().expect("sync a regular file");
    }

    #[test]
    fn only_enotsup_and_enotty_mean_no_cache() {
        for errno in [libc::ENOTSUP, libc::ENOTTY] {
            assert!(means_no_cache(&io::Error::from_raw_os_error(errno)));
        }
        for errno in [libc::ENXIO, libc::EINVAL, libc::EIO, libc::ENODEV] {
            assert!(!means_no_cache(&io::Error::from_raw_os_error(errno)));
        }
    }

    /// /dev/null refuses the flush with ENXIO, the errno a zvol whose
    /// minor is gone also returns.
    #[cfg(target_os = "illumos")]
    #[test]
    fn a_char_device_that_fails_the_flush_reports_it() {
        let file = File::options()
            .write(true)
            .open("/dev/null")
            .expect("open /dev/null");
        let disk = DiskSync::new(Arc::new(file)).expect("classify");
        let err = disk.sync().expect_err("flush of /dev/null succeeded");
        assert_eq!(err.raw_os_error(), Some(libc::ENXIO));
    }
}
