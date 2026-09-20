// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The operations that change the export.
//!
//! Every one of them is refused on a read-only share before it touches
//! the host, and every name is validated before it reaches a syscall.

use std::ffi::{CStr, CString};

use super::fuse::{self, FuseAttr, FuseEntryOut};
use super::sys::{
    build_timespec, dev_ino, fstat, openat_nofollow, parse_open_flags,
    stat_to_attr, validate_name,
};
use super::tables::{ifmt, FileType, Handle, Inode};
use super::{errno, last_os, Passthrough, PtError, PtResult};

impl Passthrough {
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

        // Linked through the descriptor rather than through a name in
        // the old parent, so the name is not resolved a second time and
        // cannot have become another file in between.
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
}
