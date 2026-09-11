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
use std::sync::Mutex;

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

fn last_os() -> PtError {
    PtError::Io(std::io::Error::last_os_error())
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
    File { fd: RawFd },
    Dir { entries: Vec<DirSnapshot> },
}

#[derive(Debug, Clone)]
struct DirSnapshot {
    name: CString,
    ino: u64,
    ftype: Option<FileType>,
}

struct HandleTable {
    by_id: HashMap<u64, Handle>,
    next_id: u64,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            by_id: HashMap::new(),
            next_id: 1,
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
}

// ---------------------------------------------------------------------------
// Passthrough core
// ---------------------------------------------------------------------------

pub struct Passthrough {
    read_only: bool,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HandleTable>,
}

impl Passthrough {
    pub fn new(path: &Path, read_only: bool) -> PtResult<Self> {
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
        let st = fstat(fd)?;
        let root = Inode {
            fd: Some(fd),
            ftype: FileType::Directory,
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
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
            inodes: Mutex::new(inodes),
            handles: Mutex::new(HandleTable::new()),
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
            _ => Err(PtError::Io(std::io::Error::from_raw_os_error(
                libc::ENOTDIR,
            ))),
        })?;

        // Probe the child with fstatat first — this tells us if it's a
        // symlink before we try to open it (open with O_NOFOLLOW would fail
        // on symlinks with ELOOP on illumos).
        let st = fstatat_nofollow(parent_fd, name)?;
        let ftype = FileType::from_mode(st.st_mode as u32)
            .ok_or(PtError::UnsupportedType)?;

        let fd_opt = match ftype {
            FileType::Regular | FileType::Directory => Some(openat_nofollow(
                parent_fd,
                name,
                if ftype == FileType::Directory {
                    libc::O_RDONLY | libc::O_DIRECTORY
                } else {
                    libc::O_RDONLY
                },
            )?),
            FileType::Symlink => None,
        };

        // Reconcile with existing cache entry if we already know this
        // (dev, ino) — keeps guest-side cache consistent.
        let dev = st.st_dev as u64;
        let ino = st.st_ino as u64;

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
            ino.fd.ok_or_else(|| {
                PtError::Io(std::io::Error::from_raw_os_error(libc::EBADF))
            })
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
                return Err(PtError::Io(std::io::Error::from_raw_os_error(
                    libc::EINVAL,
                )));
            }
            let parent = ino.sym_parent.ok_or_else(|| {
                PtError::Io(std::io::Error::from_raw_os_error(libc::EIO))
            })?;
            let name = ino.sym_name.clone().ok_or_else(|| {
                PtError::Io(std::io::Error::from_raw_os_error(libc::EIO))
            })?;
            Ok((parent, name))
        })?;
        let pfd = self.with_inode(parent_fd, |p| {
            p.fd.ok_or_else(|| {
                PtError::Io(std::io::Error::from_raw_os_error(libc::EBADF))
            })
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

    pub fn open(&self, nodeid: u64, flags: u32) -> PtResult<u64> {
        // Re-open the cached read-only inode fd with the guest's requested
        // access mode (O_RDONLY / O_WRONLY / O_RDWR plus append/trunc/…).
        let requested = flags as libc::c_int;
        let accmode = requested & libc::O_ACCMODE;
        // O_TRUNC destroys the file whatever the access mode says. POSIX
        // leaves O_RDONLY | O_TRUNC undefined, and illumos truncates. A
        // read-only share must answer EROFS the way a read-only mount does,
        // so checking the access mode alone is not enough.
        if self.read_only
            && (accmode != libc::O_RDONLY || requested & libc::O_TRUNC != 0)
        {
            return Err(PtError::ReadOnly);
        }

        let read_only = self.read_only;
        let new_fd = self.with_inode(nodeid, |ino| {
            if ino.ftype != FileType::Regular {
                return Err(PtError::Io(std::io::Error::from_raw_os_error(
                    libc::EISDIR,
                )));
            }
            let fd = ino.fd.ok_or_else(|| {
                PtError::Io(std::io::Error::from_raw_os_error(libc::EBADF))
            })?;
            // Drop flags we do not want to honor on reopen (notably
            // O_CREAT, O_DIRECTORY, O_TMPFILE). O_APPEND and O_TRUNC only
            // have meaning when writes are allowed, so a read-only share
            // keeps neither. The guard above already refuses O_TRUNC; this
            // keeps the flag away from the syscall even if that changes.
            let mut keep = libc::O_NONBLOCK;
            if !read_only {
                keep |= libc::O_APPEND | libc::O_TRUNC;
            }
            let reopen_flags = accmode | (requested & keep);
            reopen_fd(fd, reopen_flags)
        })?;

        let mut handles = self.handles.lock().unwrap();
        let id = handles.alloc_id();
        handles.by_id.insert(id, Handle::File { fd: new_fd });
        Ok(id)
    }

    /// FUSE_CREATE: atomic create-and-open.
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

        let parent_fd = self.parent_fd(parent)?;

        let accmode = (flags as libc::c_int) & libc::O_ACCMODE;
        let modifier = (flags as libc::c_int)
            & (libc::O_APPEND
                | libc::O_TRUNC
                | libc::O_EXCL
                | libc::O_NONBLOCK);
        let open_flags = accmode
            | modifier
            | libc::O_CREAT
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC;
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
        // Successfully created + opened.
        let st = match fstat(fh_fd) {
            Ok(s) => s,
            Err(e) => {
                unsafe { libc::close(fh_fd) };
                return Err(e);
            }
        };

        // Obtain a stable O_RDONLY fd for the inode cache.
        let ino_fd = match openat_nofollow(parent_fd, name, libc::O_RDONLY) {
            Ok(fd) => fd,
            Err(e) => {
                unsafe { libc::close(fh_fd) };
                return Err(e);
            }
        };

        let dev = st.st_dev as u64;
        let ino = st.st_ino as u64;
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

        let fh = {
            let mut handles = self.handles.lock().unwrap();
            let id = handles.alloc_id();
            handles.by_id.insert(id, Handle::File { fd: fh_fd });
            id
        };

        // third return: whether we actually created (always true on this
        // path; FUSE_CREATE does not have a "found existing" case since
        // O_CREAT without O_EXCL opens the existing file).
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
            ino.fd.ok_or_else(|| {
                PtError::Io(std::io::Error::from_raw_os_error(libc::EBADF))
            })
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
                ino.fd.ok_or_else(|| {
                    PtError::Io(std::io::Error::from_raw_os_error(libc::EBADF))
                })
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
                Some(_) => {
                    return Err(PtError::Io(std::io::Error::from_raw_os_error(
                        libc::EISDIR,
                    )))
                }
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
        let mut handles = self.handles.lock().unwrap();
        let handle = handles
            .by_id
            .remove(&fh)
            .ok_or(PtError::UnknownHandle(fh))?;
        match handle {
            Handle::File { fd, .. } => {
                unsafe { libc::close(fd) };
            }
            Handle::Dir { .. } => {
                return Err(PtError::Io(std::io::Error::from_raw_os_error(
                    libc::EBADF,
                )));
            }
        }
        Ok(())
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> PtResult<Vec<u8>> {
        let fd = {
            let handles = self.handles.lock().unwrap();
            match handles.by_id.get(&fh) {
                Some(Handle::File { fd, .. }) => *fd,
                Some(_) => {
                    return Err(PtError::Io(std::io::Error::from_raw_os_error(
                        libc::EISDIR,
                    )))
                }
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
                return Err(PtError::Io(std::io::Error::from_raw_os_error(
                    libc::ENOTDIR,
                )));
            }
            let fd = ino.fd.ok_or_else(|| {
                PtError::Io(std::io::Error::from_raw_os_error(libc::EBADF))
            })?;
            let dup = unsafe { libc::dup(fd) };
            if dup < 0 {
                return Err(last_os());
            }
            Ok(dup)
        })?;

        // Snapshot via fdopendir. Consumes dir_fd.
        let dirp = unsafe { libc::fdopendir(dir_fd) };
        if dirp.is_null() {
            unsafe { libc::close(dir_fd) };
            return Err(last_os());
        }

        let mut entries = Vec::new();
        loop {
            // Per POSIX readdir() is not thread-safe in general, but we hold
            // our own DIR* and serialize access via the device worker.
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
            let name = CString::new(name_bytes)
                .map_err(|_| PtError::InvalidName(name_bytes.to_vec()))?;

            entries.push(DirSnapshot {
                name,
                ino: dirent.d_ino as u64,
                ftype: None, // populated lazily for readdirplus
            });
        }

        // closedir also closes the underlying fd.
        unsafe { libc::closedir(dirp) };

        let mut handles = self.handles.lock().unwrap();
        let id = handles.alloc_id();
        handles.by_id.insert(id, Handle::Dir { entries });
        Ok(id)
    }

    pub fn releasedir(&self, fh: u64) -> PtResult<()> {
        let mut handles = self.handles.lock().unwrap();
        let handle = handles
            .by_id
            .remove(&fh)
            .ok_or(PtError::UnknownHandle(fh))?;
        match handle {
            Handle::Dir { .. } => Ok(()),
            Handle::File { fd, .. } => {
                unsafe { libc::close(fd) };
                Err(PtError::Io(std::io::Error::from_raw_os_error(libc::EBADF)))
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
            Some(Handle::Dir { entries, .. }) => entries.clone(),
            Some(_) => {
                return Err(PtError::Io(std::io::Error::from_raw_os_error(
                    libc::ENOTDIR,
                )))
            }
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
                    p.fd.ok_or_else(|| {
                        PtError::Io(std::io::Error::from_raw_os_error(
                            libc::EBADF,
                        ))
                    })
                })?;
                fstatat_nofollow(pfd, name.as_c_str())
            }
            _ => Err(PtError::Io(std::io::Error::from_raw_os_error(
                libc::ESTALE,
            ))),
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
            Some(_) => Err(PtError::Io(std::io::Error::from_raw_os_error(
                libc::ENOTDIR,
            ))),
            None => {
                Err(PtError::Io(std::io::Error::from_raw_os_error(libc::EBADF)))
            }
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

fn openat_nofollow(
    parent: RawFd,
    name: &CStr,
    flags: libc::c_int,
) -> PtResult<RawFd> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
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
            .create(fuse::FUSE_ROOT_ID, &name, libc::O_RDWR as u32, 0o644, 0)
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
