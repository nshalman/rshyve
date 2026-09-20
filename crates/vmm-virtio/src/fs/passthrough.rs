// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Host filesystem passthrough for virtio-fs.
//!
//! ## Security model
//!
//! All filesystem access is relative to `root_fd`, obtained by
//! `open(path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)` at
//! construction.
//!
//! Every descriptor opens with `O_CLOEXEC`. The VMM replaces its own
//! process image to reboot a guest, and an inherited fd would survive
//! into the new image where nothing can reach it or close it.
//!
//! Every access uses `*at()` syscalls with `O_NOFOLLOW` / `AT_SYMLINK_NOFOLLOW`
//! so symlinks are never traversed on the host side. Symlink resolution is
//! left to the guest client (FUSE returns `S_IFLNK`, client issues READLINK
//! then LOOKUP with the target).
//!
//! Every LOOKUP validates the requested name:
//! * rejects empty
//! * rejects `.` and `..`
//! * rejects any `/`
//! * rejects embedded NUL
//!
//! Device / FIFO / socket nodes are rejected at LOOKUP time; only regular
//! files, directories, and symlinks are exposed.
//!
//! ## Inode & file-handle caching
//!
//! * An `Inode` pins one fd per cached filesystem object. FUSE_FORGET drops
//!   the `lookups` refcount; on zero we close the fd.
//! * FUSE_OPEN / FUSE_OPENDIR allocate `Handle` entries backing a freshly
//!   `openat`'d fd. FUSE_RELEASE / FUSE_RELEASEDIR drop them.
//! * Directory contents are snapshotted at `OPENDIR` time — one consistent
//!   view for the life of the handle.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::fuse::{self, FuseAttr, FuseEntryOut, FuseStatfsOut};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum PtError {
    #[error("io error: {0}")]
    Io(std::io::Error),
    #[error("invalid name: {0:?}")]
    InvalidName(Vec<u8>),
    #[error("unknown nodeid {0}")]
    UnknownNode(u64),
    #[error("unknown handle {0}")]
    UnknownHandle(u64),
    #[error("read-only passthrough: operation not allowed")]
    ReadOnly,
    #[error("unsupported file type")]
    UnsupportedType,
}

impl PtError {
    /// Convert a `PtError` into a POSIX errno (positive integer, as used in
    /// FUSE response `error` field which is the negative of this).
    pub fn to_errno(&self) -> i32 {
        match self {
            PtError::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
            PtError::InvalidName(_) => libc::EINVAL,
            PtError::UnknownNode(_) => libc::ESTALE,
            PtError::UnknownHandle(_) => libc::EBADF,
            PtError::ReadOnly => libc::EROFS,
            PtError::UnsupportedType => libc::ENOTSUP,
        }
    }
}

type PtResult<T> = Result<T, PtError>;

/// The directory itself, for an `openat` relative to its own fd.
const DOT_DIR: &CStr = c".";

fn last_os() -> PtError {
    PtError::Io(std::io::Error::last_os_error())
}

fn errno(code: i32) -> PtError {
    PtError::Io(std::io::Error::from_raw_os_error(code))
}

/// Bounds on the host state one guest may hold through this device.
///
/// Every cached inode of a regular file or directory and every open
/// file handle pins one host fd until the guest sends FORGET or
/// RELEASE, and a guest decides when that is. The VMM has no fd to
/// spare for a guest that never does: past the process limit its
/// control socket, console and hotplug paths fail too. So the tables
/// are capped, and the guest sees ENFILE or EMFILE the way it would
/// on any full system.
#[derive(Debug, Clone, Copy)]
pub struct PtLimits {
    /// Cached inodes, root included.
    pub max_inodes: usize,
    /// Open file and directory handles together.
    pub max_handles: usize,
    /// Bytes of directory snapshots across every open directory handle.
    pub max_snapshot_bytes: usize,
}

impl Default for PtLimits {
    /// Sized against the 65536 fds an illumos process may hold once
    /// [`raise_fd_limit`] has run: inodes and file handles together
    /// take at most 24576 of them, leaving the rest to the VMM.
    fn default() -> Self {
        Self {
            max_inodes: 16384,
            max_handles: 8192,
            max_snapshot_bytes: 64 * 1024 * 1024,
        }
    }
}

// ---------------------------------------------------------------------------
// Inode table
// ---------------------------------------------------------------------------

/// File-type bits, widened to the type FUSE puts on the wire.
///
/// `libc`'s `S_IF*` constants have type `mode_t`, which is 16-bit on some
/// targets and 32-bit on others. Widen them once so no call site carries a
/// cast that one host calls necessary and another calls redundant.
#[allow(clippy::unnecessary_cast)]
mod ifmt {
    pub const MASK: u32 = libc::S_IFMT as u32;
    pub const REG: u32 = libc::S_IFREG as u32;
    pub const DIR: u32 = libc::S_IFDIR as u32;
    pub const LNK: u32 = libc::S_IFLNK as u32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
}

impl FileType {
    fn from_mode(mode: u32) -> Option<Self> {
        match mode & ifmt::MASK {
            m if m == ifmt::REG => Some(FileType::Regular),
            m if m == ifmt::DIR => Some(FileType::Directory),
            m if m == ifmt::LNK => Some(FileType::Symlink),
            _ => None,
        }
    }

    fn dt_type(self) -> u32 {
        match self {
            FileType::Regular => 8,   // DT_REG
            FileType::Directory => 4, // DT_DIR
            FileType::Symlink => 10,  // DT_LNK
        }
    }
}

#[derive(Debug)]
struct Inode {
    /// For regular + directory: open fd. For symlinks: None (use parent + name).
    fd: Option<RawFd>,
    ftype: FileType,
    dev: u64,
    ino: u64,
    lookups: u64,
    /// Parent nodeid for symlinks; unused otherwise. We hold an extra lookup
    /// on the parent to keep its fd alive.
    sym_parent: Option<u64>,
    sym_name: Option<CString>,
}

struct InodeTable {
    by_id: HashMap<u64, Inode>,
    by_devino: HashMap<(u64, u64), u64>,
    next_id: u64,
}

impl InodeTable {
    fn new() -> Self {
        Self {
            by_id: HashMap::new(),
            by_devino: HashMap::new(),
            // nodeid 1 is reserved for root; allocator starts at 2.
            next_id: 2,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id < 2 {
            // Wrap-around: skip reserved values (0, 1, and u64::MAX).
            self.next_id = 2;
        }
        id
    }
}

// ---------------------------------------------------------------------------
// Handle table (open files + dirs)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Handle {
    File {
        fd: RawFd,
    },
    /// Shared with every READDIR that reads it, so a listing copies
    /// the pointer and not the directory.
    Dir {
        entries: Arc<[DirSnapshot]>,
        bytes: usize,
    },
}

#[derive(Debug, Clone)]
struct DirSnapshot {
    name: CString,
    ino: u64,
    ftype: Option<FileType>,
}

impl DirSnapshot {
    /// What one entry costs against [`PtLimits::max_snapshot_bytes`].
    fn cost(&self) -> usize {
        size_of::<Self>() + self.name.as_bytes_with_nul().len()
    }
}

struct HandleTable {
    by_id: HashMap<u64, Handle>,
    next_id: u64,
    /// Sum of `bytes` over every `Handle::Dir`.
    snapshot_bytes: usize,
    limits: PtLimits,
}

impl HandleTable {
    fn new(limits: PtLimits) -> Self {
        Self {
            by_id: HashMap::new(),
            next_id: 1,
            snapshot_bytes: 0,
            limits,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        id
    }

    /// Room left for another directory snapshot.
    fn snapshot_budget(&self) -> usize {
        self.limits
            .max_snapshot_bytes
            .saturating_sub(self.snapshot_bytes)
    }

    /// Add a handle, or refuse it and close what it carries.
    fn insert(&mut self, handle: Handle) -> PtResult<u64> {
        if self.by_id.len() >= self.limits.max_handles {
            if let Handle::File { fd } = handle {
                unsafe { libc::close(fd) };
            }
            return Err(errno(libc::EMFILE));
        }
        if let Handle::Dir { bytes, .. } = &handle {
            if *bytes > self.snapshot_budget() {
                return Err(errno(libc::ENOMEM));
            }
            self.snapshot_bytes += *bytes;
        }
        let id = self.alloc_id();
        self.by_id.insert(id, handle);
        Ok(id)
    }

    fn remove(&mut self, fh: u64) -> PtResult<Handle> {
        let handle =
            self.by_id.remove(&fh).ok_or(PtError::UnknownHandle(fh))?;
        if let Handle::Dir { bytes, .. } = &handle {
            self.snapshot_bytes = self.snapshot_bytes.saturating_sub(*bytes);
        }
        Ok(handle)
    }
}

// ---------------------------------------------------------------------------
// Passthrough core
// ---------------------------------------------------------------------------

pub struct Passthrough {
    read_only: bool,
    limits: PtLimits,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HandleTable>,
}

impl Passthrough {
    pub fn new(path: &Path, read_only: bool) -> PtResult<Self> {
        Self::with_limits(path, read_only, PtLimits::default())
    }

    pub fn with_limits(
        path: &Path,
        read_only: bool,
        limits: PtLimits,
    ) -> PtResult<Self> {
        let cpath = path_cstring(path)?;
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(last_os());
        }

        let mut inodes = InodeTable::new();
        let (dev, ino) = dev_ino(&fstat(fd)?);
        let root = Inode {
            fd: Some(fd),
            ftype: FileType::Directory,
            dev,
            ino,
            lookups: u64::MAX / 2, // pin forever
            sym_parent: None,
            sym_name: None,
        };
        inodes
            .by_devino
            .insert((root.dev, root.ino), fuse::FUSE_ROOT_ID);
        inodes.by_id.insert(fuse::FUSE_ROOT_ID, root);

        Ok(Self {
            read_only,
            limits,
            inodes: Mutex::new(inodes),
            handles: Mutex::new(HandleTable::new(limits)),
        })
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    // -------------------------------------------------------------------
    // FUSE operations
    // -------------------------------------------------------------------

    pub fn lookup(&self, parent: u64, name: &CStr) -> PtResult<FuseEntryOut> {
        let name_bytes = name.to_bytes();
        validate_name(name_bytes)?;

        let parent_fd = self.with_inode(parent, |ino| match ino.fd {
            Some(fd) if ino.ftype == FileType::Directory => Ok(fd),
            _ => Err(errno(libc::ENOTDIR)),
        })?;

        // Probe the child with fstatat first — this tells us if it's a
        // symlink before we try to open it (open with O_NOFOLLOW would fail
        // on symlinks with ELOOP on illumos).
        let probe = fstatat_nofollow(parent_fd, name)?;
        let ftype = FileType::from_mode(probe.st_mode as u32)
            .ok_or(PtError::UnsupportedType)?;

        let (fd_opt, st) = open_child(parent_fd, name, &probe, ftype)?;

        // Reconcile with existing cache entry if we already know this
        // (dev, ino) — keeps guest-side cache consistent.
        let (dev, ino) = dev_ino(&st);

        let mut inodes = self.inodes.lock().unwrap();
        let nodeid = if let Some(&existing) = inodes.by_devino.get(&(dev, ino))
        {
            // Close the fd we just opened; we keep the existing one.
            if let Some(fd) = fd_opt {
                unsafe { libc::close(fd) };
            }
            if let Some(entry) = inodes.by_id.get_mut(&existing) {
                entry.lookups = entry.lookups.saturating_add(1);
            }
            existing
        } else {
            if inodes.by_id.len() >= self.limits.max_inodes {
                if let Some(fd) = fd_opt {
                    unsafe { libc::close(fd) };
                }
                return Err(errno(libc::ENFILE));
            }
            let id = inodes.alloc_id();
            let (sym_parent, sym_name) = if ftype == FileType::Symlink {
                (Some(parent), Some(name.to_owned()))
            } else {
                (None, None)
            };
            if let Some(sp) = sym_parent {
                if let Some(p) = inodes.by_id.get_mut(&sp) {
                    p.lookups = p.lookups.saturating_add(1);
                }
            }
            let new = Inode {
                fd: fd_opt,
                ftype,
                dev,
                ino,
                lookups: 1,
                sym_parent,
                sym_name,
            };
            inodes.by_id.insert(id, new);
            inodes.by_devino.insert((dev, ino), id);
            id
        };

        Ok(FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: 1,
            attr_valid: 1,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: stat_to_attr(&st),
        })
    }

    pub fn forget(&self, nodeid: u64, nlookup: u64) {
        if nodeid == fuse::FUSE_ROOT_ID {
            return;
        }
        let mut inodes = self.inodes.lock().unwrap();
        let Some(entry) = inodes.by_id.get_mut(&nodeid) else {
            return;
        };
        if nlookup >= entry.lookups {
            let fd = entry.fd;
            let dev = entry.dev;
            let ino = entry.ino;
            let parent = entry.sym_parent;
            inodes.by_id.remove(&nodeid);
            inodes.by_devino.remove(&(dev, ino));
            if let Some(fd) = fd {
                unsafe { libc::close(fd) };
            }
            if let Some(p) = parent {
                drop(inodes);
                self.forget(p, 1);
            }
        } else {
            entry.lookups -= nlookup;
        }
    }

    pub fn getattr(&self, nodeid: u64) -> PtResult<FuseAttr> {
        let st = self.stat_node(nodeid)?;
        Ok(stat_to_attr(&st))
    }

    pub fn statfs(&self, nodeid: u64) -> PtResult<FuseStatfsOut> {
        let fd = self.with_inode(nodeid, |ino| {
            ino.fd.ok_or_else(|| errno(libc::EBADF))
        })?;
        let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstatvfs(fd, &mut vfs) };
        if rc != 0 {
            return Err(last_os());
        }
        Ok(FuseStatfsOut {
            blocks: vfs.f_blocks as u64,
            bfree: vfs.f_bfree as u64,
            bavail: vfs.f_bavail as u64,
            files: vfs.f_files as u64,
            ffree: vfs.f_ffree as u64,
            bsize: vfs.f_bsize as u32,
            namelen: vfs.f_namemax as u32,
            frsize: vfs.f_frsize as u32,
            padding: 0,
            spare: [0; 6],
        })
    }

    pub fn access(&self, nodeid: u64, mask: u32) -> PtResult<()> {
        // Naive: if we can stat it, grant. A full implementation would check
        // mode bits against uid/gid. For read-only passthrough this is fine;
        // tighten in phase 2.
        let _ = self.stat_node(nodeid)?;
        let _ = mask;
        Ok(())
    }

    pub fn readlink(&self, nodeid: u64) -> PtResult<Vec<u8>> {
        let (parent_fd, name) = self.with_inode(nodeid, |ino| {
            if ino.ftype != FileType::Symlink {
                return Err(errno(libc::EINVAL));
            }
            let parent = ino.sym_parent.ok_or_else(|| errno(libc::EIO))?;
            let name = ino.sym_name.clone().ok_or_else(|| errno(libc::EIO))?;
            Ok((parent, name))
        })?;
        let pfd = self.with_inode(parent_fd, |p| {
            p.fd.ok_or_else(|| errno(libc::EBADF))
        })?;

        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        let n = unsafe {
            libc::readlinkat(
                pfd,
                name.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if n < 0 {
            return Err(last_os());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    /// FUSE_OPEN. `flags` is the guest's Linux `O_*` value.
    pub fn open(&self, nodeid: u64, flags: u32) -> PtResult<u64> {
        let flags = parse_open_flags(flags)?;
        // A read-only share must answer EROFS the way a read-only mount
        // does, and `mutates` knows that O_TRUNC destroys the file
        // whatever the access mode says.
        if self.read_only && flags.mutates() {
            return Err(PtError::ReadOnly);
        }

        let new_fd = self.with_inode(nodeid, |ino| {
            if ino.ftype != FileType::Regular {
                return Err(errno(libc::EISDIR));
            }
            let fd = ino.fd.ok_or_else(|| errno(libc::EBADF))?;
            // The data-path flags only. The file already exists and its
            // identity is fixed by `fd`, so O_CREAT, O_EXCL, O_DIRECTORY
            // and O_NOFOLLOW have nothing to say on a reopen.
            reopen_fd(fd, flags.host_io_flags())
        })?;

        self.handles
            .lock()
            .unwrap()
            .insert(Handle::File { fd: new_fd })
    }

    /// FUSE_CREATE: atomic create-and-open. `flags` is the guest's Linux
    /// `O_*` value.
    pub fn create(
        &self,
        parent: u64,
        name: &CStr,
        flags: u32,
        mode: u32,
        umask: u32,
    ) -> PtResult<(FuseEntryOut, u64, bool)> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let flags = parse_open_flags(flags)?;

        let parent_fd = self.parent_fd(parent)?;

        // O_EXCL is the one creation flag with a meaning here: it is how
        // a guest takes a lock file against another writer of the share.
        // O_NONBLOCK because O_CREAT opens a name that already exists,
        // and the export may hold a FIFO under it: without the flag the
        // open waits for a peer and the one FUSE worker never returns.
        let mut open_flags = flags.host_io_flags()
            | libc::O_CREAT
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC;
        if flags.excl {
            open_flags |= libc::O_EXCL;
        }
        let effective_mode = (mode & !umask) & 0o7777;

        let fh_fd = unsafe {
            libc::openat(
                parent_fd,
                name.as_ptr(),
                open_flags,
                effective_mode as libc::c_int,
            )
        };
        if fh_fd < 0 {
            return Err(last_os());
        }
        let st = fstat(fh_fd).inspect_err(|_| {
            unsafe { libc::close(fh_fd) };
        })?;
        // O_CREAT opens whatever the name already holds, so the type
        // comes from the descriptor rather than from the assumption
        // that this call made a regular file.
        if FileType::from_mode(st.st_mode as u32) != Some(FileType::Regular) {
            unsafe { libc::close(fh_fd) };
            return Err(PtError::UnsupportedType);
        }

        // Obtain a stable O_RDONLY fd for the inode cache. The name is
        // resolved a second time, so the two descriptors are checked to
        // be one file: the export is shared, and anything with write
        // access to it can swap the name in between.
        let ino_fd = openat_nofollow(parent_fd, name, libc::O_RDONLY)
            .inspect_err(|_| {
                unsafe { libc::close(fh_fd) };
            })?;
        let ino_st = fstat(ino_fd).inspect_err(|_| {
            unsafe { libc::close(ino_fd) };
            unsafe { libc::close(fh_fd) };
        })?;
        if ino_st.st_dev != st.st_dev || ino_st.st_ino != st.st_ino {
            unsafe { libc::close(ino_fd) };
            unsafe { libc::close(fh_fd) };
            return Err(errno(libc::ESTALE));
        }

        let (dev, ino) = dev_ino(&st);
        let nodeid = {
            let mut inodes = self.inodes.lock().unwrap();
            if let Some(&existing) = inodes.by_devino.get(&(dev, ino)) {
                // Surprisingly, this can happen with O_EXCL absent: guest
                // may race two creates. Keep the existing ino_fd.
                unsafe { libc::close(ino_fd) };
                if let Some(entry) = inodes.by_id.get_mut(&existing) {
                    entry.lookups = entry.lookups.saturating_add(1);
                }
                existing
            } else {
                if inodes.by_id.len() >= self.limits.max_inodes {
                    drop(inodes);
                    unsafe { libc::close(ino_fd) };
                    unsafe { libc::close(fh_fd) };
                    return Err(errno(libc::ENFILE));
                }
                let id = inodes.alloc_id();
                inodes.by_id.insert(
                    id,
                    Inode {
                        fd: Some(ino_fd),
                        ftype: FileType::Regular,
                        dev,
                        ino,
                        lookups: 1,
                        sym_parent: None,
                        sym_name: None,
                    },
                );
                inodes.by_devino.insert((dev, ino), id);
                id
            }
        };

        let entry = FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: 1,
            attr_valid: 1,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: stat_to_attr(&st),
        };

        // The inode entry above holds a lookup the guest will FORGET
        // whatever happens to the handle, so a refused handle leaves
        // the table consistent.
        let fh = self
            .handles
            .lock()
            .unwrap()
            .insert(Handle::File { fd: fh_fd })?;

        // FUSE_CREATE has no "found existing" case: O_CREAT without
        // O_EXCL opens the existing file, so this is always true.
        Ok((entry, fh, true))
    }

    /// FUSE_MKNOD: restricted to regular files.
    pub fn mknod(
        &self,
        parent: u64,
        name: &CStr,
        mode: u32,
        _rdev: u32,
        umask: u32,
    ) -> PtResult<FuseEntryOut> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let ftype = mode & ifmt::MASK;
        if ftype != 0 && ftype != ifmt::REG {
            return Err(PtError::UnsupportedType);
        }
        let parent_fd = self.parent_fd(parent)?;
        let effective_mode = (mode & !umask) & 0o7777;
        let fd = unsafe {
            libc::openat(
                parent_fd,
                name.as_ptr(),
                libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_RDONLY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
                effective_mode as libc::c_int,
            )
        };
        if fd < 0 {
            return Err(last_os());
        }
        unsafe { libc::close(fd) };
        self.lookup(parent, name)
    }

    /// FUSE_MKDIR.
    pub fn mkdir(
        &self,
        parent: u64,
        name: &CStr,
        mode: u32,
        umask: u32,
    ) -> PtResult<FuseEntryOut> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let parent_fd = self.parent_fd(parent)?;
        let effective_mode = (mode & !umask) & 0o7777;
        let rc = unsafe {
            libc::mkdirat(
                parent_fd,
                name.as_ptr(),
                effective_mode as libc::mode_t,
            )
        };
        if rc != 0 {
            return Err(last_os());
        }
        self.lookup(parent, name)
    }

    /// FUSE_UNLINK.
    pub fn unlink(&self, parent: u64, name: &CStr) -> PtResult<()> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let parent_fd = self.parent_fd(parent)?;
        let rc = unsafe { libc::unlinkat(parent_fd, name.as_ptr(), 0) };
        if rc != 0 {
            return Err(last_os());
        }
        Ok(())
    }

    /// FUSE_RMDIR.
    pub fn rmdir(&self, parent: u64, name: &CStr) -> PtResult<()> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let parent_fd = self.parent_fd(parent)?;
        let rc = unsafe {
            libc::unlinkat(parent_fd, name.as_ptr(), libc::AT_REMOVEDIR)
        };
        if rc != 0 {
            return Err(last_os());
        }
        Ok(())
    }

    /// FUSE_RENAME / FUSE_RENAME2.
    ///
    /// `_flags` is the RENAME_* mask from rename2. We always call renameat
    /// (flag-less); callers may return ENOSYS upstream if they see flags
    /// set that we don't support.
    pub fn rename(
        &self,
        old_parent: u64,
        old_name: &CStr,
        new_parent: u64,
        new_name: &CStr,
        _flags: u32,
    ) -> PtResult<()> {
        validate_name(old_name.to_bytes())?;
        validate_name(new_name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let old_pfd = self.parent_fd(old_parent)?;
        let new_pfd = self.parent_fd(new_parent)?;
        let rc = unsafe {
            libc::renameat(
                old_pfd,
                old_name.as_ptr(),
                new_pfd,
                new_name.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(last_os());
        }
        Ok(())
    }

    /// FUSE_SYMLINK.
    pub fn symlink(
        &self,
        parent: u64,
        name: &CStr,
        target: &CStr,
    ) -> PtResult<FuseEntryOut> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        if target.to_bytes().is_empty() {
            return Err(PtError::InvalidName(Vec::new()));
        }
        let parent_fd = self.parent_fd(parent)?;
        let rc = unsafe {
            libc::symlinkat(target.as_ptr(), parent_fd, name.as_ptr())
        };
        if rc != 0 {
            return Err(last_os());
        }
        self.lookup(parent, name)
    }

    /// FUSE_LINK — hard link.
    pub fn link(
        &self,
        old_nodeid: u64,
        new_parent: u64,
        new_name: &CStr,
    ) -> PtResult<FuseEntryOut> {
        validate_name(new_name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let old_fd = self.with_inode(old_nodeid, |ino| {
            if ino.ftype != FileType::Regular {
                return Err(PtError::UnsupportedType);
            }
            ino.fd.ok_or_else(|| errno(libc::EBADF))
        })?;
        let new_parent_fd = self.parent_fd(new_parent)?;

        // linkat(AT_FDCWD, "/proc/self/fd/N", new_parent, new_name,
        // AT_SYMLINK_FOLLOW) duplicates the fd's inode without needing to
        // re-resolve a name in old_parent.
        let path = CString::new(format!("/proc/self/fd/{old_fd}")).unwrap();
        let rc = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                path.as_ptr(),
                new_parent_fd,
                new_name.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if rc != 0 {
            return Err(last_os());
        }
        self.lookup(new_parent, new_name)
    }

    /// FUSE_SETATTR.
    ///
    /// Applies the subset of fields indicated by `valid`. Prefers the open
    /// file handle if one is provided; otherwise uses the inode fd.
    pub fn setattr(
        &self,
        nodeid: u64,
        valid: u32,
        fh: Option<u64>,
        size: u64,
        mode: u32,
        uid: u32,
        gid: u32,
        atime: (u64, u32, bool), // (sec, nsec, now)
        mtime: (u64, u32, bool),
    ) -> PtResult<FuseAttr> {
        if self.read_only {
            return Err(PtError::ReadOnly);
        }

        // Pick an fd to operate on: prefer the open handle.
        let fd = if let Some(h) = fh {
            let handles = self.handles.lock().unwrap();
            match handles.by_id.get(&h) {
                Some(Handle::File { fd, .. }) => *fd,
                _ => return Err(PtError::UnknownHandle(h)),
            }
        } else {
            self.with_inode(nodeid, |ino| {
                ino.fd.ok_or_else(|| errno(libc::EBADF))
            })?
        };

        if valid & fuse::FATTR_MODE != 0 {
            let rc =
                unsafe { libc::fchmod(fd, (mode & 0o7777) as libc::mode_t) };
            if rc != 0 {
                return Err(last_os());
            }
        }
        if valid & (fuse::FATTR_UID | fuse::FATTR_GID) != 0 {
            let u = if valid & fuse::FATTR_UID != 0 {
                uid as libc::uid_t
            } else {
                (-1i32) as libc::uid_t
            };
            let g = if valid & fuse::FATTR_GID != 0 {
                gid as libc::gid_t
            } else {
                (-1i32) as libc::gid_t
            };
            let rc = unsafe { libc::fchown(fd, u, g) };
            if rc != 0 {
                return Err(last_os());
            }
        }
        if valid & fuse::FATTR_SIZE != 0 {
            let rc = unsafe { libc::ftruncate(fd, size as libc::off_t) };
            if rc != 0 {
                return Err(last_os());
            }
        }
        if valid
            & (fuse::FATTR_ATIME
                | fuse::FATTR_MTIME
                | fuse::FATTR_ATIME_NOW
                | fuse::FATTR_MTIME_NOW)
            != 0
        {
            let a = build_timespec(
                valid & fuse::FATTR_ATIME != 0,
                atime.2,
                atime.0,
                atime.1,
            );
            let m = build_timespec(
                valid & fuse::FATTR_MTIME != 0,
                mtime.2,
                mtime.0,
                mtime.1,
            );
            let times = [a, m];
            let rc = unsafe { libc::futimens(fd, times.as_ptr()) };
            if rc != 0 {
                return Err(last_os());
            }
        }

        let st = fstat(fd)?;
        Ok(stat_to_attr(&st))
    }

    /// FUSE_WRITE.
    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> PtResult<u32> {
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let fd = {
            let handles = self.handles.lock().unwrap();
            match handles.by_id.get(&fh) {
                Some(Handle::File { fd, .. }) => *fd,
                Some(_) => return Err(errno(libc::EISDIR)),
                None => return Err(PtError::UnknownHandle(fh)),
            }
        };
        let n = unsafe {
            libc::pwrite(
                fd,
                data.as_ptr() as *const libc::c_void,
                data.len(),
                offset as libc::off_t,
            )
        };
        if n < 0 {
            return Err(last_os());
        }
        Ok(n as u32)
    }

    /// FUSE_FSYNC / FUSE_FSYNCDIR.
    pub fn fsync(&self, fh: u64, datasync: bool) -> PtResult<()> {
        let fd = {
            let handles = self.handles.lock().unwrap();
            match handles.by_id.get(&fh) {
                Some(Handle::File { fd, .. }) => *fd,
                Some(Handle::Dir { .. }) => {
                    // No fd for snapshotted dirs; sync is a no-op.
                    return Ok(());
                }
                None => return Err(PtError::UnknownHandle(fh)),
            }
        };
        let rc = if datasync {
            #[cfg(any(
                target_os = "linux",
                target_os = "illumos",
                target_os = "solaris"
            ))]
            unsafe {
                libc::fdatasync(fd)
            }
            #[cfg(not(any(
                target_os = "linux",
                target_os = "illumos",
                target_os = "solaris"
            )))]
            unsafe {
                libc::fsync(fd)
            }
        } else {
            unsafe { libc::fsync(fd) }
        };
        if rc != 0 {
            return Err(last_os());
        }
        Ok(())
    }

    pub fn release(&self, fh: u64) -> PtResult<()> {
        let handle = self.handles.lock().unwrap().remove(fh)?;
        match handle {
            Handle::File { fd, .. } => {
                unsafe { libc::close(fd) };
            }
            Handle::Dir { .. } => {
                return Err(errno(libc::EBADF));
            }
        }
        Ok(())
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> PtResult<Vec<u8>> {
        let fd = {
            let handles = self.handles.lock().unwrap();
            match handles.by_id.get(&fh) {
                Some(Handle::File { fd, .. }) => *fd,
                Some(_) => return Err(errno(libc::EISDIR)),
                None => return Err(PtError::UnknownHandle(fh)),
            }
        };
        let mut buf = vec![0u8; size as usize];
        let n = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                offset as libc::off_t,
            )
        };
        if n < 0 {
            return Err(last_os());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    pub fn opendir(&self, nodeid: u64) -> PtResult<u64> {
        let dir_fd = self.with_inode(nodeid, |ino| {
            if ino.ftype != FileType::Directory {
                return Err(errno(libc::ENOTDIR));
            }
            let fd = ino.fd.ok_or_else(|| errno(libc::EBADF))?;
            // A `dup` would share the cached fd's open file description
            // and with it the directory offset, which readdir leaves at
            // EOF for every later OPENDIR of the same inode. Opening "."
            // gets a description of its own.
            openat_nofollow(fd, DOT_DIR, libc::O_RDONLY | libc::O_DIRECTORY)
        })?;

        // Consumes dir_fd.
        let dirp = unsafe { libc::fdopendir(dir_fd) };
        if dirp.is_null() {
            unsafe { libc::close(dir_fd) };
            return Err(last_os());
        }

        // Read against the room left now, so a directory too big for
        // the budget is refused before it is held in memory. The FUSE
        // worker is the only caller, so the room cannot shrink under
        // this loop; `insert` checks again regardless.
        let budget = self.handles.lock().unwrap().snapshot_budget();
        let mut bytes = 0usize;
        let mut entries = Vec::new();
        loop {
            set_errno(0);
            let entp = unsafe { libc::readdir(dirp) };
            if entp.is_null() {
                let errno = get_errno();
                if errno != 0 {
                    unsafe { libc::closedir(dirp) };
                    return Err(PtError::Io(
                        std::io::Error::from_raw_os_error(errno),
                    ));
                }
                break;
            }

            let dirent = unsafe { &*entp };
            let name_ptr = dirent.d_name.as_ptr();
            // SAFETY: d_name is NUL-terminated by dirent semantics.
            let name_cstr =
                unsafe { CStr::from_ptr(name_ptr as *const libc::c_char) };
            let name_bytes = name_cstr.to_bytes();
            if name_bytes == b"." || name_bytes == b".." {
                // Skip — FUSE layer doesn't want these (client handles them).
                continue;
            }
            // `d_name` is NUL-terminated, so the copy cannot hold an
            // interior NUL.
            let entry = DirSnapshot {
                name: name_cstr.to_owned(),
                ino: dirent.d_ino as u64,
                ftype: None,
            };
            bytes = bytes.saturating_add(entry.cost());
            if bytes > budget {
                unsafe { libc::closedir(dirp) };
                return Err(errno(libc::ENOMEM));
            }
            entries.push(entry);
        }

        // closedir also closes the underlying fd.
        unsafe { libc::closedir(dirp) };

        self.handles.lock().unwrap().insert(Handle::Dir {
            entries: Arc::from(entries),
            bytes,
        })
    }

    pub fn releasedir(&self, fh: u64) -> PtResult<()> {
        let handle = self.handles.lock().unwrap().remove(fh)?;
        match handle {
            Handle::Dir { .. } => Ok(()),
            Handle::File { fd, .. } => {
                unsafe { libc::close(fd) };
                Err(errno(libc::EBADF))
            }
        }
    }

    /// Iterate entries from a directory handle starting at `offset`.
    ///
    /// The callback is invoked for each entry; returning `false` stops
    /// iteration early (e.g. when the response buffer is full).
    pub fn readdir_each<F: FnMut(u64, &CStr, u32, u64) -> bool>(
        &self,
        fh: u64,
        offset: u64,
        mut f: F,
    ) -> PtResult<()> {
        let handles = self.handles.lock().unwrap();
        let snap = match handles.by_id.get(&fh) {
            Some(Handle::Dir { entries, .. }) => Arc::clone(entries),
            Some(_) => return Err(errno(libc::ENOTDIR)),
            None => return Err(PtError::UnknownHandle(fh)),
        };
        drop(handles);

        // offset is the index of the NEXT entry the client wants; we return
        // entries starting there. The per-entry `off` we write is its next
        // position (1-based).
        let start = offset as usize;
        for (idx, entry) in snap.iter().enumerate().skip(start) {
            let next_off = (idx + 1) as u64;
            let dt = entry.ftype.map(|t| t.dt_type()).unwrap_or(0); // DT_UNKNOWN
            if !f(entry.ino, entry.name.as_c_str(), dt, next_off) {
                break;
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------

    fn stat_node(&self, nodeid: u64) -> PtResult<libc::stat> {
        let (kind, fd_opt, parent_name) = self.with_inode(nodeid, |ino| {
            Ok((
                ino.ftype,
                ino.fd,
                ino.sym_parent.and_then(|p| {
                    ino.sym_name.as_ref().map(|n| (p, n.clone()))
                }),
            ))
        })?;

        match (kind, fd_opt, parent_name) {
            (_, Some(fd), _) => fstat(fd),
            (FileType::Symlink, None, Some((parent, name))) => {
                let pfd = self.with_inode(parent, |p| {
                    p.fd.ok_or_else(|| errno(libc::EBADF))
                })?;
                fstatat_nofollow(pfd, name.as_c_str())
            }
            _ => Err(errno(libc::ESTALE)),
        }
    }

    fn with_inode<R, F>(&self, nodeid: u64, f: F) -> PtResult<R>
    where
        F: FnOnce(&Inode) -> PtResult<R>,
    {
        let inodes = self.inodes.lock().unwrap();
        let ino = inodes
            .by_id
            .get(&nodeid)
            .ok_or(PtError::UnknownNode(nodeid))?;
        f(ino)
    }

    fn parent_fd(&self, nodeid: u64) -> PtResult<RawFd> {
        self.with_inode(nodeid, |ino| match ino.fd {
            Some(fd) if ino.ftype == FileType::Directory => Ok(fd),
            Some(_) => Err(errno(libc::ENOTDIR)),
            None => Err(errno(libc::EBADF)),
        })
    }

    /// Close every guest-held fd and empty the inode and handle tables,
    /// keeping the pinned root inode so the device keeps serving after a
    /// guest reset. Idempotent.
    ///
    /// `Passthrough::new` stores the root inode with `fd: Some(root_fd)`,
    /// the same descriptor as the `root_fd` field, so closing that entry
    /// would close `root_fd` out from under a live device.
    pub fn clear(&self) {
        let mut handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for h in handles.by_id.values() {
            if let Handle::File { fd, .. } = h {
                unsafe { libc::close(*fd) };
            }
        }
        handles.by_id.clear();
        handles.next_id = 1;
        handles.snapshot_bytes = 0;
        drop(handles);

        let mut inodes = self
            .inodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inodes.by_id.retain(|id, ino| {
            if *id == fuse::FUSE_ROOT_ID {
                return true;
            }
            if let Some(fd) = ino.fd {
                unsafe { libc::close(fd) };
            }
            false
        });
        inodes.by_devino.retain(|_, id| *id == fuse::FUSE_ROOT_ID);
        inodes.next_id = 2;
    }

    /// fsync every open regular-file handle. Attempts them all and then
    /// reports the first failure, so one bad fd does not hide the rest.
    pub fn sync_all(&self) -> std::io::Result<()> {
        let handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut first: Option<std::io::Error> = None;
        for h in handles.by_id.values() {
            if let Handle::File { fd, .. } = h {
                if unsafe { libc::fsync(*fd) } != 0 && first.is_none() {
                    first = Some(std::io::Error::last_os_error());
                }
            }
        }
        match first {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for Passthrough {
    fn drop(&mut self) {
        // A panic inside Drop during an unwind aborts the process, so a
        // poisoned mutex must not be unwrapped here.
        let inodes = self
            .inodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for ino in inodes.by_id.values() {
            if let Some(fd) = ino.fd {
                unsafe { libc::close(fd) };
            }
        }
        let handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for h in handles.by_id.values() {
            if let Handle::File { fd, .. } = h {
                unsafe { libc::close(*fd) };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn path_cstring(p: &Path) -> PtResult<CString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        CString::new(p.as_os_str().as_bytes()).map_err(|_| {
            PtError::InvalidName(p.to_string_lossy().into_owned().into_bytes())
        })
    }
    #[cfg(not(unix))]
    {
        CString::new(p.to_string_lossy().into_owned())
            .map_err(|_| PtError::InvalidName(Vec::new()))
    }
}

/// Validate a single path component.
///
/// * Empty → EINVAL.
/// * Contains `/` → EINVAL (caller shouldn't compose paths).
/// * Exactly `.` or `..` → EINVAL.
/// * Contains NUL → impossible via CStr, but we double-check as bytes.
pub fn validate_name(name: &[u8]) -> PtResult<()> {
    if name.is_empty()
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
        || name.contains(&0)
    {
        return Err(PtError::InvalidName(name.to_vec()));
    }
    Ok(())
}

/// Raise the soft `RLIMIT_NOFILE` to the hard limit.
///
/// The soft limit is a few thousand by default and every cached inode
/// costs one fd, so a guest listing a large tree would hit it long
/// before [`PtLimits`] does, and every fd-consuming path in the VMM
/// would fail with it. Returns the soft limit now in force.
pub fn raise_fd_limit() -> std::io::Result<libc::rlim_t> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let target = fd_limit_target(lim.rlim_max);
    if lim.rlim_cur >= target {
        return Ok(lim.rlim_cur);
    }
    lim.rlim_cur = target;
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(target)
}

/// macOS refuses a soft `RLIMIT_NOFILE` above `OPEN_MAX` even when the
/// hard limit reads as unlimited. Development hosts only; illumos takes
/// its hard limit as it stands.
#[cfg(target_os = "macos")]
fn fd_limit_target(hard: libc::rlim_t) -> libc::rlim_t {
    hard.min(10240)
}

#[cfg(not(target_os = "macos"))]
fn fd_limit_target(hard: libc::rlim_t) -> libc::rlim_t {
    hard
}

/// Translate the guest's Linux open flags, refusing bits this server
/// does not know rather than guessing at a host meaning for them.
fn parse_open_flags(flags: u32) -> PtResult<fuse::OpenFlags> {
    fuse::OpenFlags::from_linux(flags).map_err(|_| errno(libc::EINVAL))
}

fn fstat(fd: RawFd) -> PtResult<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd, &mut st) };
    if rc != 0 {
        return Err(last_os());
    }
    Ok(st)
}

fn fstatat_nofollow(parent: RawFd, name: &CStr) -> PtResult<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(parent, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW)
    };
    if rc != 0 {
        return Err(last_os());
    }
    Ok(st)
}

/// Open `name` under `parent`, never following a final symlink and
/// never waiting.
///
/// `O_NONBLOCK` is what keeps the single FUSE worker running: a FIFO
/// opened without it blocks until a peer appears at the other end, and
/// the export is allowed to contain one. The flag is inert on the
/// regular files and directories this server serves, and the caller
/// checks what it opened.
/// The `(dev, ino)` pair naming one host file.
///
/// `dev_t` and `ino_t` differ in width between illumos and the
/// development hosts, so the widening happens here once rather than at
/// every caller.
#[allow(clippy::unnecessary_cast)]
fn dev_ino(st: &libc::stat) -> (u64, u64) {
    (st.st_dev as u64, st.st_ino as u64)
}

/// Open the child the caller probed, and hand back the stat of what was
/// really opened.
///
/// The name is resolved a second time by the open, so what arrives need
/// not be what was probed: anything with write access to the export can
/// swap it in between. The descriptor is then the authority for both the
/// type and the identity, and a disagreement is reported as the stale
/// name it is. A symlink has no descriptor, so its probe stands.
fn open_child(
    parent_fd: RawFd,
    name: &CStr,
    probe: &libc::stat,
    ftype: FileType,
) -> PtResult<(Option<RawFd>, libc::stat)> {
    if ftype == FileType::Symlink {
        return Ok((None, *probe));
    }
    let flags = if ftype == FileType::Directory {
        libc::O_RDONLY | libc::O_DIRECTORY
    } else {
        libc::O_RDONLY
    };
    let fd = openat_nofollow(parent_fd, name, flags)?;
    let opened = fstat(fd).inspect_err(|_| {
        unsafe { libc::close(fd) };
    })?;
    if FileType::from_mode(opened.st_mode as u32) != Some(ftype)
        || opened.st_dev != probe.st_dev
        || opened.st_ino != probe.st_ino
    {
        unsafe { libc::close(fd) };
        return Err(errno(libc::ESTALE));
    }
    Ok((Some(fd), opened))
}

fn openat_nofollow(
    parent: RawFd,
    name: &CStr,
    flags: libc::c_int,
) -> PtResult<RawFd> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(last_os());
    }
    Ok(fd)
}

// The width of each `libc::stat` field changes between targets: `st_mode`
// and `st_nlink` are 16-bit on macOS and 32-bit on illumos, and `st_rdev`
// is signed on one and unsigned on the other. Keep every cast so the
// function builds on all of them.
#[allow(clippy::unnecessary_cast)]
pub fn stat_to_attr(st: &libc::stat) -> FuseAttr {
    FuseAttr {
        ino: st.st_ino as u64,
        size: st.st_size as u64,
        blocks: st.st_blocks as u64,
        atime: st.st_atime as u64,
        mtime: st.st_mtime as u64,
        ctime: st.st_ctime as u64,
        atimensec: nsec(st.st_atime_nsec),
        mtimensec: nsec(st.st_mtime_nsec),
        ctimensec: nsec(st.st_ctime_nsec),
        mode: st.st_mode as u32,
        nlink: st.st_nlink as u32,
        uid: st.st_uid as u32,
        gid: st.st_gid as u32,
        rdev: st.st_rdev as u32,
        blksize: st.st_blksize as u32,
        flags: 0,
    }
}

fn nsec(val: i64) -> u32 {
    (val as u32).min(999_999_999)
}

/// Re-open a file by its existing open fd, with new flags.
///
/// Uses the procfs magic symlink `/proc/self/fd/<N>` — available on Linux
/// and illumos. Avoids re-resolving a path and so is safe against
/// TOCTOU with directory renames on the host. Does NOT apply O_NOFOLLOW
/// because the magic symlink would trip ELOOP; the identity of the reopened
/// file is fixed by the source fd.
fn reopen_fd(fd: RawFd, flags: libc::c_int) -> PtResult<RawFd> {
    let path = CString::new(format!("/proc/self/fd/{fd}")).unwrap();
    let new_fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if new_fd < 0 {
        return Err(last_os());
    }
    Ok(new_fd)
}

/// Build a timespec for futimens given FUSE setattr inputs.
fn build_timespec(
    has_explicit: bool,
    want_now: bool,
    sec: u64,
    nsec: u32,
) -> libc::timespec {
    if want_now {
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        }
    } else if has_explicit {
        libc::timespec {
            tv_sec: sec as libc::time_t,
            tv_nsec: nsec as libc::c_long,
        }
    } else {
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        }
    }
}

// Portable errno accessors. Rust's `std::io::Error::last_os_error()` reads
// errno but doesn't let us clear it; we need the raw pointer for readdir.
#[cfg(any(target_os = "illumos", target_os = "solaris"))]
fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::___errno() }
}
#[cfg(target_os = "linux")]
fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}
#[cfg(target_os = "macos")]
fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

fn set_errno(v: i32) {
    unsafe { *errno_ptr() = v };
}

fn get_errno() -> i32 {
    unsafe { *errno_ptr() }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use fuse::linux_oflags as lx;

    /// The host flags on an open file handle's descriptor.
    #[cfg(unix)]
    fn handle_fl(pt: &Passthrough, fh: u64) -> libc::c_int {
        let handles = pt.handles.lock().expect("handles");
        let Some(Handle::File { fd }) = handles.by_id.get(&fh) else {
            panic!("no file handle {fh}");
        };
        let fl = unsafe { libc::fcntl(*fd, libc::F_GETFL) };
        assert!(fl >= 0, "F_GETFL failed");
        fl
    }

    // Linux O_APPEND is 0o2000, which illumos reads as O_EXCL. A create
    // that masked the wire value with host constants lost the append
    // and refused the name if it had appeared since LOOKUP.
    #[cfg(unix)]
    #[test]
    fn o_append_on_the_wire_reaches_the_host_descriptor() {
        let dir = private_dir();
        let pt = Passthrough::new(dir.path(), false).expect("open temp");
        let name = CString::new("log").expect("name");
        let flags = lx::O_WRONLY | lx::O_CREAT | lx::O_APPEND;
        let (_, fh, _) = pt
            .create(fuse::FUSE_ROOT_ID, &name, flags, 0o644, 0)
            .expect("create");
        assert_ne!(handle_fl(&pt, fh) & libc::O_APPEND, 0, "append lost");

        let flags = lx::O_WRONLY | lx::O_CREAT;
        let (_, fh, _) = pt
            .create(fuse::FUSE_ROOT_ID, &name, flags, 0o644, 0)
            .expect("reopen");
        assert_eq!(handle_fl(&pt, fh) & libc::O_APPEND, 0, "append invented");
    }

    /// A private directory for one test.
    ///
    /// `tempfile::tempdir` lets the process umask cut the mode it asks
    /// for, and the console tests move the umask from another thread.
    /// A directory created in that window loses its search bit and every
    /// `openat` below it then fails with EACCES. Set the mode back.
    #[cfg(unix)]
    fn private_dir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(
            dir.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("restore tempdir mode");
        dir
    }

    #[test]
    fn reject_empty_name() {
        assert!(validate_name(b"").is_err());
    }

    #[test]
    fn reject_dot_names() {
        assert!(validate_name(b".").is_err());
        assert!(validate_name(b"..").is_err());
    }

    #[test]
    fn reject_slash() {
        assert!(validate_name(b"a/b").is_err());
        assert!(validate_name(b"/").is_err());
        assert!(validate_name(b"../etc/passwd").is_err());
    }

    #[test]
    fn reject_nul() {
        assert!(validate_name(b"a\x00b").is_err());
    }

    #[test]
    fn accept_ordinary_names() {
        assert!(validate_name(b"foo").is_ok());
        assert!(validate_name(b"foo.txt").is_ok());
        assert!(validate_name(b"file with spaces").is_ok());
        assert!(validate_name(b".hidden").is_ok()); // dotfiles are fine
    }

    #[cfg(unix)]
    #[test]
    fn passthrough_root_opens() {
        let dir = private_dir();
        let pt = Passthrough::new(dir.path(), true).expect("open temp");
        let attr = pt.getattr(fuse::FUSE_ROOT_ID).expect("getattr root");
        assert_eq!(attr.mode & ifmt::MASK, ifmt::DIR);
    }

    // `clear` runs on guest reset and on VM teardown. It must close every
    // guest-held fd without closing `root_fd`, which is aliased into the
    // FUSE_ROOT_ID inode entry.
    #[cfg(unix)]
    #[test]
    fn clear_closes_handles_and_keeps_root() {
        let dir = private_dir();
        let pt = Passthrough::new(dir.path(), true).expect("open temp");

        let fh = pt.opendir(fuse::FUSE_ROOT_ID).expect("opendir root");
        assert!(pt.releasedir(fh).is_ok());

        let fh2 = pt.opendir(fuse::FUSE_ROOT_ID).expect("opendir root again");
        pt.clear();

        // Handle table emptied.
        assert!(matches!(pt.releasedir(fh2), Err(PtError::UnknownHandle(_))));
        // Root inode survives, so the device still serves after a reset.
        let attr = pt.getattr(fuse::FUSE_ROOT_ID).expect("root still open");
        assert_eq!(attr.mode & ifmt::MASK, ifmt::DIR);
    }

    /// Every name a directory handle serves, in snapshot order.
    #[cfg(unix)]
    fn listing(pt: &Passthrough, fh: u64) -> Vec<Vec<u8>> {
        let mut names = Vec::new();
        pt.readdir_each(fh, 0, |_, name, _, _| {
            names.push(name.to_bytes().to_vec());
            true
        })
        .expect("readdir");
        names
    }

    // The bug this guards: a `dup` of the cached inode fd shares its
    // directory offset, so the first snapshot read the directory to
    // EOF and every OPENDIR after it saw nothing.
    #[cfg(unix)]
    #[test]
    fn a_second_opendir_of_the_same_directory_sees_the_same_entries() {
        let dir = private_dir();
        for name in ["a", "b", "c"] {
            std::fs::write(dir.path().join(name), b"x").expect("seed");
        }
        let pt = Passthrough::new(dir.path(), true).expect("open temp");

        let first = pt.opendir(fuse::FUSE_ROOT_ID).expect("first opendir");
        let mut seen_first = listing(&pt, first);
        let second = pt.opendir(fuse::FUSE_ROOT_ID).expect("second opendir");
        let mut seen_second = listing(&pt, second);

        seen_first.sort();
        seen_second.sort();
        assert_eq!(seen_first, [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        assert_eq!(seen_second, seen_first, "the second snapshot differs");
    }

    /// Make a FIFO in `dir`, or say the host would not.
    #[cfg(unix)]
    fn mkfifo(dir: &Path, name: &str) {
        let path = path_cstring(&dir.join(name)).expect("fifo path");
        let rc = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
    }

    /// Run `body` on a thread and fail if it has not returned in time.
    ///
    /// A blocking `open` would hang the whole test run, so the deadline
    /// is what turns that into a failure.
    #[cfg(unix)]
    fn within<T: Send + 'static>(
        body: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(body());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the call never returned: it is waiting on a FIFO peer")
    }

    // A share may hold a FIFO, and the guest reaches it: Linux sends
    // CREATE for a name it has not cached, and O_CREAT opens whatever is
    // already there. Without O_NONBLOCK that open waits for a peer at
    // the other end and the one FUSE worker never comes back.
    #[cfg(unix)]
    #[test]
    fn a_create_over_a_fifo_is_refused_without_waiting() {
        let dir = private_dir();
        mkfifo(dir.path(), "p");
        let pt = Passthrough::new(dir.path(), false).expect("open temp");
        let refused = within(move || {
            pt.create(
                fuse::FUSE_ROOT_ID,
                &CString::new("p").expect("name"),
                lx::O_WRONLY | lx::O_CREAT,
                0o644,
                0,
            )
            .err()
            .map(|e| e.to_errno())
        });
        // ENXIO because a write-only open of a FIFO with no reader
        // fails at once once O_NONBLOCK is set, ENOTSUP if the host
        // lets the open through and the type check refuses it. Either
        // answer reaches the guest; a third one never comes back.
        assert!(
            matches!(refused, Some(libc::ENXIO | libc::ENOTSUP)),
            "unexpected refusal: {refused:?}"
        );
    }

    // The same name through LOOKUP. The type check comes off the probe
    // here, so the guest is refused before anything is opened.
    #[cfg(unix)]
    #[test]
    fn a_lookup_of_a_fifo_is_refused_without_waiting() {
        let dir = private_dir();
        mkfifo(dir.path(), "p");
        let pt = Passthrough::new(dir.path(), false).expect("open temp");
        let refused = within(move || {
            pt.lookup(fuse::FUSE_ROOT_ID, &CString::new("p").expect("name"))
                .err()
                .map(|e| e.to_errno())
        });
        assert_eq!(refused, Some(libc::ENOTSUP));
    }

    // The probe and the open resolve the name twice, so a type the
    // descriptor disagrees with is the name having changed underneath.
    #[cfg(unix)]
    #[test]
    fn a_child_that_changed_type_between_probe_and_open_is_stale() {
        let dir = private_dir();
        mkfifo(dir.path(), "p");
        let parent = Passthrough::new(dir.path(), false).expect("open temp");
        let parent_fd = parent
            .inodes
            .lock()
            .unwrap()
            .by_id
            .get(&fuse::FUSE_ROOT_ID)
            .and_then(|ino| ino.fd)
            .expect("root fd");
        let probe = fstat(parent_fd).expect("stat the root");
        let refused = within(move || {
            open_child(
                parent_fd,
                &CString::new("p").expect("name"),
                &probe,
                FileType::Regular,
            )
            .err()
            .map(|e| e.to_errno())
        });
        assert_eq!(refused, Some(libc::ESTALE));
    }

    #[cfg(unix)]
    fn limited(dir: &tempfile::TempDir, limits: PtLimits) -> Passthrough {
        Passthrough::with_limits(dir.path(), false, limits).expect("open temp")
    }

    #[cfg(unix)]
    fn name(s: &str) -> CString {
        CString::new(s).expect("name")
    }

    // Every cached inode pins a host fd until the guest FORGETs it, and
    // the guest decides when that is.
    #[cfg(unix)]
    #[test]
    fn the_inode_table_is_capped_and_forget_makes_room() {
        let dir = private_dir();
        for n in ["a", "b"] {
            std::fs::write(dir.path().join(n), b"x").expect("seed");
        }
        // Root takes one of the two.
        let pt = limited(
            &dir,
            PtLimits {
                max_inodes: 2,
                ..PtLimits::default()
            },
        );

        let a = pt.lookup(fuse::FUSE_ROOT_ID, &name("a")).expect("a fits");
        let refused = pt.lookup(fuse::FUSE_ROOT_ID, &name("b")).unwrap_err();
        assert_eq!(refused.to_errno(), libc::ENFILE);
        // A hit on a cached inode costs no fd and is still served.
        pt.lookup(fuse::FUSE_ROOT_ID, &name("a")).expect("a again");

        pt.forget(a.nodeid, 2);
        pt.lookup(fuse::FUSE_ROOT_ID, &name("b"))
            .expect("b fits now");
    }

    #[cfg(unix)]
    #[test]
    fn create_is_held_to_the_inode_cap() {
        let dir = private_dir();
        let pt = limited(
            &dir,
            PtLimits {
                max_inodes: 1,
                ..PtLimits::default()
            },
        );
        let flags = lx::O_WRONLY | lx::O_CREAT;
        let refused = pt
            .create(fuse::FUSE_ROOT_ID, &name("n"), flags, 0o644, 0)
            .unwrap_err();
        assert_eq!(refused.to_errno(), libc::ENFILE);
        assert!(pt.handles.lock().unwrap().by_id.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_handle_table_is_capped_and_release_makes_room() {
        let dir = private_dir();
        let pt = limited(
            &dir,
            PtLimits {
                max_handles: 1,
                ..PtLimits::default()
            },
        );
        let fh = pt.opendir(fuse::FUSE_ROOT_ID).expect("first fits");
        let refused = pt.opendir(fuse::FUSE_ROOT_ID).unwrap_err();
        assert_eq!(refused.to_errno(), libc::EMFILE);
        pt.releasedir(fh).expect("release");
        pt.opendir(fuse::FUSE_ROOT_ID).expect("fits again");
    }

    // A directory is snapshotted whole at OPENDIR, and the guest picks
    // how many snapshots it holds.
    #[cfg(unix)]
    #[test]
    fn directory_snapshots_are_held_to_a_byte_budget() {
        let dir = private_dir();
        for i in 0..8 {
            std::fs::write(dir.path().join(format!("f{i}")), b"x")
                .expect("seed");
        }
        // Room for one snapshot of this directory, not two.
        let one = 8 * (size_of::<DirSnapshot>() + 3);
        let pt = limited(
            &dir,
            PtLimits {
                max_snapshot_bytes: one + one / 2,
                ..PtLimits::default()
            },
        );
        let fh = pt.opendir(fuse::FUSE_ROOT_ID).expect("first fits");
        let refused = pt.opendir(fuse::FUSE_ROOT_ID).unwrap_err();
        assert_eq!(refused.to_errno(), libc::ENOMEM);
        pt.releasedir(fh).expect("release");
        pt.opendir(fuse::FUSE_ROOT_ID).expect("fits again");
        pt.clear();
        assert_eq!(pt.handles.lock().unwrap().snapshot_bytes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn readdir_shares_one_snapshot_across_reads() {
        let dir = private_dir();
        std::fs::write(dir.path().join("f"), b"x").expect("seed");
        let pt = Passthrough::new(dir.path(), true).expect("open temp");
        let fh = pt.opendir(fuse::FUSE_ROOT_ID).expect("opendir");
        assert_eq!(listing(&pt, fh), listing(&pt, fh));
        let handles = pt.handles.lock().unwrap();
        let Some(Handle::Dir { entries, .. }) = handles.by_id.get(&fh) else {
            panic!("no dir handle");
        };
        assert_eq!(Arc::strong_count(entries), 1, "a listing kept a copy");
    }

    #[cfg(unix)]
    #[test]
    fn the_fd_limit_is_raised_to_the_hard_limit() {
        let soft = raise_fd_limit().expect("raise");
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) },
            0
        );
        assert_eq!(lim.rlim_cur, soft);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(lim.rlim_cur, lim.rlim_max);
    }

    #[cfg(unix)]
    #[test]
    fn clear_is_idempotent() {
        let dir = private_dir();
        let pt = Passthrough::new(dir.path(), true).expect("open temp");
        pt.clear();
        pt.clear();
        assert!(pt.getattr(fuse::FUSE_ROOT_ID).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn sync_all_with_no_handles_is_ok() {
        let dir = private_dir();
        let pt = Passthrough::new(dir.path(), true).expect("open temp");
        assert!(pt.sync_all().is_ok());
    }

    // Covers the open-file branch of both methods, and proves `clear` drops
    // the devino index with the inode table. A survivor there would hand a
    // later lookup a nodeid that no longer exists.
    #[cfg(unix)]
    #[test]
    fn clear_and_sync_all_cover_file_handles() {
        let dir = private_dir();
        let pt = Passthrough::new(dir.path(), false).expect("open temp");
        let name = CString::new("data.bin").expect("name");

        let (entry, fh, _) = pt
            .create(fuse::FUSE_ROOT_ID, &name, lx::O_RDWR, 0o644, 0)
            .expect("create");
        assert!(entry.nodeid > fuse::FUSE_ROOT_ID);
        pt.sync_all().expect("sync open handle");

        pt.clear();
        assert!(matches!(pt.release(fh), Err(PtError::UnknownHandle(_))));

        // The file is still on disk, so a fresh lookup must succeed and get
        // a live nodeid rather than a dangling devino hit.
        let again = pt.lookup(fuse::FUSE_ROOT_ID, &name).expect("re-lookup");
        pt.getattr(again.nodeid).expect("re-looked-up node is live");
    }
}
