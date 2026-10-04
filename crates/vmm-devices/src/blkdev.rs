// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Durable flush and write-cache control for a disk backing fd.
//!
//! Zvols default to write-through, so every sync write commits the ZIL;
//! writeback with guest-driven flushes is what makes small writes fast.
//! Only a zvol's write cache is ever changed: its flush always commits
//! the ZIL, so neither cache state can make a flush silent. Any other
//! disk is accepted only if it flushes, or if its driver says it has no
//! cache to flush and no volatile cache is on.

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
const DKIOCINFO: libc::c_int = DKIOC | 3; // 0x0403
#[cfg(target_os = "illumos")]
const DKIOCGETWCE: libc::c_int = DKIOC | 36; // 0x0424
#[cfg(target_os = "illumos")]
const DKIOCSETWCE: libc::c_int = DKIOC | 37; // 0x0425
#[cfg(target_os = "illumos")]
const DKIOCFLUSHWRITECACHE: libc::c_int = DKIOC | 34; // 0x0422

/// The disk ioctls the flush policy rests on, so tests can inject results.
#[cfg(any(target_os = "illumos", test))]
pub(crate) trait DiskOps: Send + Sync {
    /// The drive name from DKIOCINFO (`dki_dname`).
    fn drive_name(&self) -> io::Result<String>;
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
/// How is decided once, at open. A character disk device takes
/// DKIOCFLUSHWRITECACHE rather than fsync(2): illumos fsync skips the
/// flush when no I/O marked the device since the last one, so two
/// flushes that race a write can both miss it, and it ignores the
/// result of the flush it does send.
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
    /// Classify `file` and decide how it flushes. `log` takes the
    /// best-effort zvol write cache changes that fail.
    pub fn new(
        file: Arc<File>,
        read_only: bool,
        log: &slog::Logger,
    ) -> io::Result<Self> {
        let st_mode = file.metadata()?.mode();
        #[cfg(target_os = "illumos")]
        {
            let ops = Arc::new(Dkioc(Arc::clone(&file)));
            Self::build(file, st_mode, read_only, ops, log)
        }
        #[cfg(not(target_os = "illumos"))]
        {
            let _ = log;
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
        log: &slog::Logger,
    ) -> io::Result<Self> {
        check_not_block(st_mode, read_only, None)?;
        let mode = if read_only {
            Mode::ReadOnly
        } else if st_mode & libc::S_IFMT == libc::S_IFCHR {
            Mode::Device(Device::open(ops, log)?)
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
    // Raw lofi fails its flush probe too, so do not point at it.
    let raw = match path {
        Some(p) if p.starts_with("/dev/lofi") => "a zvol".to_string(),
        _ => match path.and_then(raw_node) {
            Some(raw) => raw.display().to_string(),
            None => "the raw (character) node".to_string(),
        },
    };
    let msg = format!(
        "a writable block device node cannot report flush errors; use \
         {raw} or attach it read-only"
    );
    Err(io::Error::new(io::ErrorKind::InvalidInput, msg))
}

/// The raw node for an illumos block node under /dev: the first `dsk`
/// becomes `rdsk`. The same name elsewhere is an ordinary directory.
fn raw_node(path: &Path) -> Option<PathBuf> {
    let mut parts = path.components();
    if parts.next() != Some(Component::RootDir)
        || parts.next() != Some(Component::Normal("dev".as_ref()))
    {
        return None;
    }
    let mut found = false;
    let mut out = PathBuf::from("/dev");
    for c in parts {
        match c {
            Component::Normal(n) if !found && n == "dsk" => {
                found = true;
                let mut raw = std::ffi::OsString::from("r");
                raw.push(n);
                out.push(raw);
            }
            Component::Normal(n) => out.push(n),
            _ => return None,
        }
    }
    found.then_some(out)
}

#[cfg(any(target_os = "illumos", test))]
struct Device {
    ops: Arc<dyn DiskOps>,
    /// False only for a disk whose driver says it has no volatile
    /// cache to flush; its flushes are no-ops.
    can_flush: bool,
    /// A zvol whose write cache was off when this open checked it.
    /// Two opens can both see it off and both restore it; that is
    /// harmless because every zvol flush commits the ZIL.
    restore_wce: bool,
    log: slog::Logger,
}

#[cfg(any(target_os = "illumos", test))]
impl Device {
    fn open(ops: Arc<dyn DiskOps>, log: &slog::Logger) -> io::Result<Self> {
        let log = log.clone();
        if ops.drive_name().is_ok_and(|n| n == "zvol") {
            return Ok(Self::open_zvol(ops, log));
        }
        Self::open_disk(ops, log)
    }

    /// Best effort writeback. Another open of this zvol may restore
    /// write-through under us or leave writeback on after us; either is
    /// safe because a zvol flush always commits the ZIL.
    fn open_zvol(ops: Arc<dyn DiskOps>, log: slog::Logger) -> Self {
        let restore_wce = match ops.get_wce() {
            Ok(true) => false,
            Ok(false) => match ops.set_wce(true) {
                Ok(()) => true,
                Err(e) => {
                    slog::warn!(log, "zvol write cache stays off";
                        "error" => %e);
                    false
                }
            },
            Err(e) => {
                slog::warn!(log, "zvol write cache state unknown";
                    "error" => %e);
                false
            }
        };
        Self {
            ops,
            can_flush: true,
            restore_wce,
            log,
        }
    }

    fn open_disk(ops: Arc<dyn DiskOps>, log: slog::Logger) -> io::Result<Self> {
        let wce = match ops.get_wce() {
            Ok(on) => Some(on),
            Err(e) if unsupported(&e) => None,
            Err(e) => {
                return Err(refuse(
                    "DKIOCGETWCE failed",
                    e,
                    "attach it read-only, or use a zvol",
                ))
            }
        };
        // sd keeps its own copy of the cache state, read at attach and
        // refreshed only by SETWCE. While that copy says off it answers
        // a flush with success and sends nothing, so refresh it before
        // the probe or the probe may test nothing.
        if wce == Some(true) {
            ops.set_wce(true).map_err(|e| {
                refuse(
                    "write cache refresh failed",
                    e,
                    "sd answers EINVAL when the cache mode is not \
                     changeable or sd.conf disables the cache; attach it \
                     read-only, or turn the write cache off",
                )
            })?;
        }
        // Decided once: a cache turned on out of band later can leave
        // the driver's flag stale and its flushes silent.
        let can_flush = match ops.flush() {
            Ok(()) => true,
            Err(e) => match e.raw_os_error() {
                Some(libc::ENOTSUP) if wce != Some(true) => false,
                Some(libc::ENOTSUP) => {
                    return Err(refuse(
                        "write cache is on but the disk cannot flush it",
                        e,
                        "attach it read-only, or turn the write cache off",
                    ))
                }
                // Raw lofi: it may wrap a disk device, and specfs drops
                // that device's flush result.
                Some(libc::ENOTTY) => {
                    return Err(refuse(
                        "the device cannot report a flush",
                        e,
                        "attach it read-only, or use a zvol",
                    ))
                }
                _ => {
                    return Err(refuse(
                        "flush failed at open",
                        e,
                        "attach it read-only, or use a zvol",
                    ))
                }
            },
        };
        Ok(Self {
            ops,
            can_flush,
            restore_wce: false,
            log,
        })
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
        if self.restore_wce {
            if let Err(e) = self.ops.set_wce(false) {
                slog::warn!(self.log, "zvol write cache left on";
                    "error" => %e);
            }
        }
    }
}

/// ENOTSUP and ENOTTY: the driver has no such control. Neither proves
/// the write cache is off.
#[cfg(any(target_os = "illumos", test))]
fn unsupported(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::ENOTTY))
}

#[cfg(any(target_os = "illumos", test))]
fn refuse(what: &str, e: io::Error, next: &str) -> io::Error {
    io::Error::new(e.kind(), format!("disk refused: {what}: {e}; {next}"))
}

/// `dki_dname` up to its first NUL; all 16 bytes when it has none.
#[cfg(any(target_os = "illumos", test))]
fn dname(raw: &[u8]) -> String {
    let len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..len]).into_owned()
}

/// The ioctls on a real fd.
#[cfg(target_os = "illumos")]
struct Dkioc(Arc<File>);

#[cfg(target_os = "illumos")]
impl DiskOps for Dkioc {
    fn drive_name(&self) -> io::Result<String> {
        let mut info = DkCinfo::default();
        // SAFETY: `info` lives through the call and matches the layout
        // of struct dk_cinfo the kernel writes.
        unsafe {
            vmm_api_common::ioctl(
                self.0.as_raw_fd(),
                DKIOCINFO,
                std::ptr::from_mut(&mut info).cast(),
            )
        }?;
        Ok(dname(&info.dki_dname.map(|c| c as u8)))
    }

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

/// struct dk_cinfo from <sys/dkio.h>.
#[cfg(any(target_os = "illumos", test))]
#[cfg_attr(not(target_os = "illumos"), allow(dead_code))]
#[repr(C)]
#[derive(Default)]
struct DkCinfo {
    dki_cname: [libc::c_char; 16],
    dki_ctype: libc::c_ushort,
    dki_flags: libc::c_ushort,
    dki_cnum: libc::c_ushort,
    dki_addr: libc::c_uint,
    dki_space: libc::c_uint,
    dki_prio: libc::c_uint,
    dki_vec: libc::c_uint,
    dki_dname: [libc::c_char; 16],
    dki_unit: libc::c_uint,
    dki_slave: libc::c_uint,
    dki_partition: libc::c_ushort,
    dki_maxtransfer: libc::c_ushort,
}

// sizeof (struct dk_cinfo) from the illumos headers. It has no longs,
// so the size holds on any host.
#[cfg(any(target_os = "illumos", test))]
const _: () = assert!(size_of::<DkCinfo>() == 68);

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
