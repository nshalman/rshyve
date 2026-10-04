// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Durable flush and write-cache control for a disk backing fd.
//!
//! Zvols default to write-through, so every sync write commits the ZIL;
//! writeback with guest-driven flushes is what makes small writes fast.
//!
//! A disk that cannot flush is accepted only with its write cache
//! confirmed off, or with no cache control at all. The write cache is
//! device-global and other opens of the device may be attached, so this
//! code only ever moves it toward off after open.

use std::fs::{File, OpenOptions};
use std::io;
#[cfg(target_os = "illumos")]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
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

/// Open a disk backing path, refusing a writable block node by name.
pub fn open_disk(path: &Path, read_only: bool) -> io::Result<File> {
    let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
    check_not_block(file.metadata()?.mode(), read_only, Some(path))?;
    Ok(file)
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
    /// A regular file. On illumos `sync_data` is fsync, and fsync of a
    /// ZFS file commits the ZIL.
    Fsync,
    #[cfg(any(target_os = "illumos", test))]
    Device(Device),
}

impl DiskSync {
    /// Classify `file`, then probe its flush and set its write cache.
    pub fn new(file: Arc<File>, read_only: bool) -> io::Result<Self> {
        let st_mode = file.metadata()?.mode();
        #[cfg(target_os = "illumos")]
        {
            let ops = Arc::new(Dkioc(Arc::clone(&file)));
            Self::build(file, st_mode, read_only, ops)
        }
        #[cfg(not(target_os = "illumos"))]
        {
            check_not_block(st_mode, read_only, None)?;
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
        check_not_block(st_mode, read_only, None)?;
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

/// Refuse a writable block node. Specfs answers ENOTTY to every disk
/// ioctl on it, and writes wait in its page cache (spec_write starts
/// async writeback only for full MAXBSIZE blocks). spec_fsync drains
/// that cache and tries the device flush but drops both errors, and
/// after one ENOTSUP or ENOTTY it never flushes the node again.
fn check_not_block(
    st_mode: u32,
    read_only: bool,
    path: Option<&Path>,
) -> io::Result<()> {
    if read_only || st_mode & libc::S_IFMT != libc::S_IFBLK {
        return Ok(());
    }
    let raw = match path.and_then(raw_node) {
        Some(raw) => raw.display().to_string(),
        None => "the raw (character) node".to_string(),
    };
    let msg = format!(
        "a writable block device node cannot report flush errors; use \
         {raw} or attach it read-only"
    );
    Err(io::Error::new(io::ErrorKind::InvalidInput, msg))
}

/// The raw node for an illumos block node path: `dsk` becomes `rdsk`
/// and `lofi` becomes `rlofi`.
fn raw_node(path: &Path) -> Option<PathBuf> {
    let mut found = false;
    let out = path
        .components()
        .map(|c| match c {
            Component::Normal(n) if !found && (n == "dsk" || n == "lofi") => {
                found = true;
                let mut raw = std::ffi::OsString::from("r");
                raw.push(n);
                raw
            }
            c => c.as_os_str().to_os_string(),
        })
        .collect::<PathBuf>();
    found.then_some(out)
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
                dev.can_flush = false;
                dev.cache_off_as_found()?;
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
        match dev.ops.get_wce() {
            Ok(false) => {
                if dev.ops.set_wce(true).is_err() {
                    return Ok(dev);
                }
                dev.enabled_wce = true;
            }
            // sd keeps its own copy of the cache state, read at attach
            // and refreshed only by SETWCE. While that copy says off it
            // answers a flush with success and sends nothing, so the
            // probe above may have tested nothing.
            Ok(true) => {
                let _ = dev.ops.set_wce(true);
            }
            Err(_) => return Ok(dev),
        }
        match dev.ops.flush() {
            Ok(()) => Ok(dev),
            Err(e) if cannot_flush(&e) => {
                dev.can_flush = false;
                dev.turn_cache_off()?;
                dev.enabled_wce = false;
                Ok(dev)
            }
            // Drop turns the cache back off if this open turned it on.
            Err(e) => Err(io::Error::new(
                e.kind(),
                format!("disk flush failed with its write cache on: {e}"),
            )),
        }
    }

    /// The first probe cannot flush and the cache is as this open found
    /// it. GETWCE that is unsupported from the start means no cache
    /// control at all: the nvme blkdev namespace without a volatile
    /// cache answers this way.
    fn cache_off_as_found(&self) -> io::Result<()> {
        match self.ops.get_wce() {
            Ok(false) => Ok(()),
            Err(e) if cannot_flush(&e) => Ok(()),
            Err(e) => Err(refuse(e)),
            Ok(true) => self.turn_cache_off(),
        }
    }

    /// Turn the write cache off and read it back, for good: turning it
    /// back on later could expose another open of this device that also
    /// cannot flush. GETWCE worked earlier in this open, so any error on
    /// the read-back, ENOTSUP and ENOTTY included, refuses the disk.
    fn turn_cache_off(&self) -> io::Result<()> {
        self.ops.set_wce(false).map_err(refuse)?;
        if self.ops.get_wce().map_err(refuse)? {
            return Err(refuse(io::Error::other(
                "write cache still on after DKIOCSETWCE=0",
            )));
        }
        Ok(())
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
