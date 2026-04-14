// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Block-device helpers: write-cache control (DKIOCSETWCE).
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
//! Drop order: declare `DiskCache` fields *before* the owning `File` in
//! the parent struct so the restore ioctl runs while the fd is still open.

#[cfg(target_os = "illumos")]
use std::io;
use std::os::fd::RawFd;

#[cfg(target_os = "illumos")]
const DKIOC: libc::c_int = 0x04 << 8; // 0x0400
#[cfg(target_os = "illumos")]
const DKIOCGETWCE: libc::c_int = DKIOC | 36; // 0x0424
#[cfg(target_os = "illumos")]
const DKIOCSETWCE: libc::c_int = DKIOC | 37; // 0x0425

/// Holds zvol write-cache state for the lifetime of a backing fd.
///
/// `initial == Some(false)` means we enabled writeback and must restore
/// on Drop. `Some(true)` means the device was already in writeback.
/// `None` means we never touched it (read-only, non-illumos, or a device
/// that didn't support DKIOCSETWCE).
pub struct DiskCache {
    #[cfg_attr(not(target_os = "illumos"), allow(dead_code))]
    fd: RawFd,
    #[cfg_attr(not(target_os = "illumos"), allow(dead_code))]
    initial: Option<bool>,
}

impl DiskCache {
    /// Probe and (where supported) enable writeback on the backing fd.
    ///
    /// Best-effort: any ioctl failure leaves the device state unchanged.
    pub fn new(fd: RawFd, read_only: bool) -> Self {
        if read_only {
            return Self { fd, initial: None };
        }
        Self::new_inner(fd)
    }

    #[cfg(target_os = "illumos")]
    fn new_inner(fd: RawFd) -> Self {
        let mut v: libc::c_int = 0;
        if dkioc_wce(fd, DKIOCGETWCE, &mut v).is_err() {
            // Not a disk device, or the backing store doesn't expose WCE
            // (regular files on illumos, iSCSI/COMSTAR targets, etc.).
            return Self { fd, initial: None };
        }
        let initial = v != 0;
        if initial {
            // Already in writeback; nothing to do, nothing to restore.
            return Self { fd, initial: None };
        }
        let mut enable: libc::c_int = 1;
        if dkioc_wce(fd, DKIOCSETWCE, &mut enable).is_err() {
            // GET worked but SET refused — leave state alone.
            return Self { fd, initial: None };
        }
        Self { fd, initial: Some(false) }
    }

    #[cfg(not(target_os = "illumos"))]
    fn new_inner(fd: RawFd) -> Self {
        Self { fd, initial: None }
    }
}

impl Drop for DiskCache {
    fn drop(&mut self) {
        #[cfg(target_os = "illumos")]
        {
            if self.initial == Some(false) {
                let mut v: libc::c_int = 0;
                let _ = dkioc_wce(self.fd, DKIOCSETWCE, &mut v);
            }
        }
    }
}

/// Typed wrapper around the variadic `libc::ioctl`.
///
/// Passes `&mut c_int` explicitly so the integer doesn't hit variadic
/// type promotion and the kernel reads/writes the expected 4 bytes.
#[cfg(target_os = "illumos")]
fn dkioc_wce(fd: RawFd, cmd: libc::c_int, v: &mut libc::c_int) -> io::Result<()> {
    // SAFETY: caller holds `fd` open for the duration of the call; `v`
    // lives through the call and is the correct size for DKIOC{GET,SET}WCE.
    let r = unsafe { libc::ioctl(fd, cmd, v as *mut libc::c_int) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
