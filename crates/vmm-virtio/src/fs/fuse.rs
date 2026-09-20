// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FUSE protocol types and constants.
//!
//! Targets FUSE ABI 7.33. All on-the-wire structures are packed little-endian
//! (host and guest are both little-endian on x86_64).

#![allow(dead_code)]

// ---------------------------------------------------------------------------
// Protocol version
// ---------------------------------------------------------------------------

pub const FUSE_KERNEL_VERSION: u32 = 7;
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 33;

// ---------------------------------------------------------------------------
// Special inode numbers
// ---------------------------------------------------------------------------

pub const FUSE_ROOT_ID: u64 = 1;

// ---------------------------------------------------------------------------
// Opcodes
// ---------------------------------------------------------------------------

pub const FUSE_LOOKUP: u32 = 1;
pub const FUSE_FORGET: u32 = 2;
pub const FUSE_GETATTR: u32 = 3;
pub const FUSE_SETATTR: u32 = 4;
pub const FUSE_READLINK: u32 = 5;
pub const FUSE_SYMLINK: u32 = 6;
pub const FUSE_MKNOD: u32 = 8;
pub const FUSE_MKDIR: u32 = 9;
pub const FUSE_UNLINK: u32 = 10;
pub const FUSE_RMDIR: u32 = 11;
pub const FUSE_RENAME: u32 = 12;
pub const FUSE_LINK: u32 = 13;
pub const FUSE_OPEN: u32 = 14;
pub const FUSE_READ: u32 = 15;
pub const FUSE_WRITE: u32 = 16;
pub const FUSE_STATFS: u32 = 17;
pub const FUSE_RELEASE: u32 = 18;
pub const FUSE_FSYNC: u32 = 20;
pub const FUSE_SETXATTR: u32 = 21;
pub const FUSE_GETXATTR: u32 = 22;
pub const FUSE_LISTXATTR: u32 = 23;
pub const FUSE_REMOVEXATTR: u32 = 24;
pub const FUSE_FLUSH: u32 = 25;
pub const FUSE_INIT: u32 = 26;
pub const FUSE_OPENDIR: u32 = 27;
pub const FUSE_READDIR: u32 = 28;
pub const FUSE_RELEASEDIR: u32 = 29;
pub const FUSE_FSYNCDIR: u32 = 30;
pub const FUSE_GETLK: u32 = 31;
pub const FUSE_SETLK: u32 = 32;
pub const FUSE_SETLKW: u32 = 33;
pub const FUSE_ACCESS: u32 = 34;
pub const FUSE_CREATE: u32 = 35;
pub const FUSE_INTERRUPT: u32 = 36;
pub const FUSE_BMAP: u32 = 37;
pub const FUSE_DESTROY: u32 = 38;
pub const FUSE_BATCH_FORGET: u32 = 42;
pub const FUSE_READDIRPLUS: u32 = 44;
pub const FUSE_RENAME2: u32 = 45;
pub const FUSE_LSEEK: u32 = 46;
pub const FUSE_COPY_FILE_RANGE: u32 = 47;
pub const FUSE_SYNCFS: u32 = 50;

// ---------------------------------------------------------------------------
// INIT feature flags
// ---------------------------------------------------------------------------

pub const FUSE_ASYNC_READ: u32 = 1 << 0;
pub const FUSE_POSIX_LOCKS: u32 = 1 << 1;
pub const FUSE_FILE_OPS: u32 = 1 << 2;
pub const FUSE_ATOMIC_O_TRUNC: u32 = 1 << 3;
pub const FUSE_EXPORT_SUPPORT: u32 = 1 << 4;
pub const FUSE_BIG_WRITES: u32 = 1 << 5;
pub const FUSE_DONT_MASK: u32 = 1 << 6;
pub const FUSE_SPLICE_WRITE: u32 = 1 << 7;
pub const FUSE_SPLICE_MOVE: u32 = 1 << 8;
pub const FUSE_SPLICE_READ: u32 = 1 << 9;
pub const FUSE_FLOCK_LOCKS: u32 = 1 << 10;
pub const FUSE_HAS_IOCTL_DIR: u32 = 1 << 11;
pub const FUSE_AUTO_INVAL_DATA: u32 = 1 << 12;
pub const FUSE_DO_READDIRPLUS: u32 = 1 << 13;
pub const FUSE_READDIRPLUS_AUTO: u32 = 1 << 14;
pub const FUSE_ASYNC_DIO: u32 = 1 << 15;
pub const FUSE_WRITEBACK_CACHE: u32 = 1 << 16;
pub const FUSE_NO_OPEN_SUPPORT: u32 = 1 << 17;
pub const FUSE_PARALLEL_DIROPS: u32 = 1 << 18;
pub const FUSE_HANDLE_KILLPRIV: u32 = 1 << 19;
pub const FUSE_POSIX_ACL: u32 = 1 << 20;
pub const FUSE_ABORT_ERROR: u32 = 1 << 21;
pub const FUSE_MAX_PAGES: u32 = 1 << 22;
pub const FUSE_CACHE_SYMLINKS: u32 = 1 << 23;
pub const FUSE_NO_OPENDIR_SUPPORT: u32 = 1 << 24;
pub const FUSE_EXPLICIT_INVAL_DATA: u32 = 1 << 25;
pub const FUSE_MAP_ALIGNMENT: u32 = 1 << 26;
pub const FUSE_SUBMOUNTS: u32 = 1 << 27;

// ---------------------------------------------------------------------------
// SETATTR valid mask
// ---------------------------------------------------------------------------

pub const FATTR_MODE: u32 = 1 << 0;
pub const FATTR_UID: u32 = 1 << 1;
pub const FATTR_GID: u32 = 1 << 2;
pub const FATTR_SIZE: u32 = 1 << 3;
pub const FATTR_ATIME: u32 = 1 << 4;
pub const FATTR_MTIME: u32 = 1 << 5;
pub const FATTR_FH: u32 = 1 << 6;
pub const FATTR_ATIME_NOW: u32 = 1 << 7;
pub const FATTR_MTIME_NOW: u32 = 1 << 8;
pub const FATTR_LOCKOWNER: u32 = 1 << 9;
pub const FATTR_CTIME: u32 = 1 << 10;

// ---------------------------------------------------------------------------
// Open flags (in fuse_open_in.flags)
// ---------------------------------------------------------------------------

pub const FOPEN_DIRECT_IO: u32 = 1 << 0;
pub const FOPEN_KEEP_CACHE: u32 = 1 << 1;
pub const FOPEN_NONSEEKABLE: u32 = 1 << 2;
pub const FOPEN_CACHE_DIR: u32 = 1 << 3;
pub const FOPEN_STREAM: u32 = 1 << 4;

// ---------------------------------------------------------------------------
// Common message layout
// ---------------------------------------------------------------------------

/// Every FUSE request begins with this header.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub padding: u32,
}

pub const FUSE_IN_HEADER_SIZE: usize = 40;
const _: () = assert!(size_of::<FuseInHeader>() == FUSE_IN_HEADER_SIZE);

/// Every FUSE response begins with this header.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseOutHeader {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

pub const FUSE_OUT_HEADER_SIZE: usize = 16;
const _: () = assert!(size_of::<FuseOutHeader>() == FUSE_OUT_HEADER_SIZE);

// ---------------------------------------------------------------------------
// Attribute structures
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    pub flags: u32,
}

const _: () = assert!(size_of::<FuseAttr>() == 88);

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseEntryOut {
    pub nodeid: u64,
    pub generation: u64,
    pub entry_valid: u64,
    pub attr_valid: u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: FuseAttr,
}

const _: () = assert!(size_of::<FuseEntryOut>() == 128);

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseAttrOut {
    pub attr_valid: u64,
    pub attr_valid_nsec: u32,
    pub dummy: u32,
    pub attr: FuseAttr,
}

const _: () = assert!(size_of::<FuseAttrOut>() == 104);

// ---------------------------------------------------------------------------
// Per-opcode request / reply bodies
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInitIn {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseInitOut {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write: u32,
    pub time_gran: u32,
    pub max_pages: u16,
    pub map_alignment: u16,
    pub unused: [u32; 8],
}

const _: () = assert!(size_of::<FuseInitOut>() == 64);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseForgetIn {
    pub nlookup: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseGetattrIn {
    pub flags: u32,
    pub dummy: u32,
    pub fh: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseOpenIn {
    pub flags: u32,
    pub unused: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseOpenOut {
    pub fh: u64,
    pub open_flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseReleaseIn {
    pub fh: u64,
    pub flags: u32,
    pub release_flags: u32,
    pub lock_owner: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseReadIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub read_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseWriteIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub write_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseWriteOut {
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseFlushIn {
    pub fh: u64,
    pub unused: u32,
    pub padding: u32,
    pub lock_owner: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseFsyncIn {
    pub fh: u64,
    pub fsync_flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseAccessIn {
    pub mask: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseGetxattrIn {
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseGetxattrOut {
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseStatfsOut {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
    pub padding: u32,
    pub spare: [u32; 6],
}

const _: () = assert!(size_of::<FuseStatfsOut>() == 80);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseMkdirIn {
    pub mode: u32,
    pub umask: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseCreateIn {
    pub flags: u32,
    pub mode: u32,
    pub umask: u32,
    pub open_flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseCreateOut {
    pub entry: FuseEntryOut,
    pub open: FuseOpenOut,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseSetattrIn {
    pub valid: u32,
    pub padding: u32,
    pub fh: u64,
    pub size: u64,
    pub lock_owner: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub unused4: u32,
    pub uid: u32,
    pub gid: u32,
    pub unused5: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseRenameIn {
    pub newdir: u64,
}

/// Dirent header written by READDIR; followed by `namelen` bytes of name and
/// padding to a 64-bit boundary.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FuseDirent {
    pub ino: u64,
    pub off: u64,
    pub namelen: u32,
    pub typ: u32,
}

pub const FUSE_DIRENT_SIZE: usize = 24;
const _: () = assert!(size_of::<FuseDirent>() == FUSE_DIRENT_SIZE);

/// Aligned length of a dirent including a name of `namelen` bytes.
pub fn dirent_size(namelen: usize) -> usize {
    (FUSE_DIRENT_SIZE + namelen + 7) & !7
}

// ---------------------------------------------------------------------------
// Open flags
// ---------------------------------------------------------------------------

/// Linux `O_*` values, as `fuse_open_in.flags` and `fuse_create_in.flags`
/// carry them.
///
/// The guest is Linux, so the numbers on the wire are Linux's whatever
/// host this server runs on. The host's `libc::O_*` differ: on illumos
/// `O_APPEND` is 0x08 and `O_EXCL` is 0x400, so masking the wire value
/// with host constants turned a guest `O_CREAT|O_EXCL` into
/// `O_CREAT|O_NONBLOCK` and a guest `O_APPEND` into `O_EXCL`.
pub mod linux_oflags {
    pub const O_ACCMODE: u32 = 0o3;
    pub const O_RDONLY: u32 = 0o0;
    pub const O_WRONLY: u32 = 0o1;
    pub const O_RDWR: u32 = 0o2;
    pub const O_CREAT: u32 = 0o100;
    pub const O_EXCL: u32 = 0o200;
    pub const O_NOCTTY: u32 = 0o400;
    pub const O_TRUNC: u32 = 0o1000;
    pub const O_APPEND: u32 = 0o2000;
    pub const O_NONBLOCK: u32 = 0o4000;
    pub const O_DSYNC: u32 = 0o10000;
    pub const O_DIRECT: u32 = 0o40000;
    pub const O_LARGEFILE: u32 = 0o100000;
    pub const O_DIRECTORY: u32 = 0o200000;
    pub const O_NOFOLLOW: u32 = 0o400000;
    pub const O_NOATIME: u32 = 0o1000000;
    pub const O_CLOEXEC: u32 = 0o2000000;
    /// `O_SYNC` is `__O_SYNC | O_DSYNC` on Linux.
    pub const O_SYNC: u32 = 0o4010000;
}

/// A guest's open flags, translated to the host's numbering.
///
/// Every bit the guest may set is either translated or named as
/// ignored below; anything else is refused, so a flag this server has
/// not thought about never reaches `openat` with a host meaning the
/// guest did not ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlags {
    /// One of the host's `O_RDONLY`, `O_WRONLY` or `O_RDWR`.
    pub accmode: libc::c_int,
    pub creat: bool,
    pub excl: bool,
    pub trunc: bool,
    pub append: bool,
    pub nonblock: bool,
    pub sync: bool,
    pub dsync: bool,
    pub directory: bool,
    pub nofollow: bool,
}

/// Linux bits with no effect on the host side of a passthrough.
///
/// `O_LARGEFILE` is the only file size on a 64-bit host, `O_NOATIME`
/// and `O_DIRECT` are advisory, `O_NOCTTY` is for terminals, and
/// `O_CLOEXEC` is set on every descriptor here regardless.
const IGNORED_LINUX_OFLAGS: u32 = linux_oflags::O_LARGEFILE
    | linux_oflags::O_NOATIME
    | linux_oflags::O_DIRECT
    | linux_oflags::O_NOCTTY
    | linux_oflags::O_CLOEXEC;

impl OpenFlags {
    /// Translate Linux open flags. `Err` carries the bits this server
    /// does not know.
    pub fn from_linux(flags: u32) -> Result<Self, u32> {
        use linux_oflags as lx;
        let accmode = match flags & lx::O_ACCMODE {
            lx::O_RDONLY => libc::O_RDONLY,
            lx::O_WRONLY => libc::O_WRONLY,
            lx::O_RDWR => libc::O_RDWR,
            _ => return Err(flags & lx::O_ACCMODE),
        };
        let known = lx::O_ACCMODE
            | lx::O_CREAT
            | lx::O_EXCL
            | lx::O_TRUNC
            | lx::O_APPEND
            | lx::O_NONBLOCK
            | lx::O_SYNC
            | lx::O_DSYNC
            | lx::O_DIRECTORY
            | lx::O_NOFOLLOW
            | IGNORED_LINUX_OFLAGS;
        let unknown = flags & !known;
        if unknown != 0 {
            return Err(unknown);
        }
        let has = |bit: u32| flags & bit == bit;
        Ok(Self {
            accmode,
            creat: has(lx::O_CREAT),
            excl: has(lx::O_EXCL),
            trunc: has(lx::O_TRUNC),
            append: has(lx::O_APPEND),
            nonblock: has(lx::O_NONBLOCK),
            sync: has(lx::O_SYNC),
            dsync: has(lx::O_DSYNC),
            directory: has(lx::O_DIRECTORY),
            nofollow: has(lx::O_NOFOLLOW),
        })
    }

    /// Whether this open can change the file.
    ///
    /// `O_TRUNC` truncates alongside `O_RDONLY` on illumos, so the
    /// access mode alone does not say.
    pub fn mutates(&self) -> bool {
        self.accmode != libc::O_RDONLY || self.trunc || self.creat
    }

    /// The host flags for the data path: the access mode and the
    /// modifiers that change how reads and writes behave. Creation
    /// and lookup flags are the caller's to add.
    pub fn host_io_flags(&self) -> libc::c_int {
        let mut out = self.accmode;
        if self.append {
            out |= libc::O_APPEND;
        }
        if self.trunc {
            out |= libc::O_TRUNC;
        }
        if self.nonblock {
            out |= libc::O_NONBLOCK;
        }
        if self.sync {
            out |= libc::O_SYNC;
        }
        if self.dsync {
            out |= libc::O_DSYNC;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Helpers for zero-copy access
// ---------------------------------------------------------------------------

/// Interpret a prefix of `buf` as `T` (little-endian packed).
///
/// Returns `None` if `buf` is too short. Uses a bytewise copy so the result
/// is valid even if `buf` is misaligned.
pub fn read_at<T: Copy>(buf: &[u8]) -> Option<T> {
    let need = size_of::<T>();
    if buf.len() < need {
        return None;
    }
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    unsafe {
        std::ptr::copy_nonoverlapping(
            buf.as_ptr(),
            out.as_mut_ptr() as *mut u8,
            need,
        );
        Some(out.assume_init())
    }
}

/// Serialize `val` by bytewise copy into a freshly allocated vector.
pub fn bytes_of<T: Copy>(val: &T) -> Vec<u8> {
    let len = size_of::<T>();
    let mut v = vec![0u8; len];
    unsafe {
        std::ptr::copy_nonoverlapping(
            val as *const T as *const u8,
            v.as_mut_ptr(),
            len,
        );
    }
    v
}

/// Append a `#[repr(C)] Copy` value to a byte buffer.
pub fn push_val<T: Copy>(buf: &mut Vec<u8>, val: &T) {
    let len = size_of::<T>();
    let start = buf.len();
    buf.resize(start + len, 0);
    unsafe {
        std::ptr::copy_nonoverlapping(
            val as *const T as *const u8,
            buf.as_mut_ptr().add(start),
            len,
        );
    }
}

// ---------------------------------------------------------------------------
// Errno translation
// ---------------------------------------------------------------------------

/// Linux errno values, named so the table below reads as a mapping.
///
/// FUSE is a Linux protocol, so the number in a reply header is read with
/// Linux numbering whatever host this server runs on. errno 1..=34 is the
/// classic UNIX set and agrees on every host, so only the range above it
/// needs naming here.
mod linux_errno {
    pub const EDEADLK: i32 = 35;
    pub const ENAMETOOLONG: i32 = 36;
    pub const ENOLCK: i32 = 37;
    pub const ENOSYS: i32 = 38;
    pub const ENOTEMPTY: i32 = 39;
    pub const ELOOP: i32 = 40;
    pub const ENOMSG: i32 = 42;
    pub const EIDRM: i32 = 43;
    pub const ENODATA: i32 = 61;
    pub const ENOLINK: i32 = 67;
    pub const EPROTO: i32 = 71;
    pub const EMULTIHOP: i32 = 72;
    pub const EBADMSG: i32 = 74;
    pub const EOVERFLOW: i32 = 75;
    pub const EILSEQ: i32 = 84;
    pub const EOPNOTSUPP: i32 = 95;
    pub const ESTALE: i32 = 116;
    pub const EDQUOT: i32 = 122;
    pub const ECANCELED: i32 = 125;
    /// Fallback for a host errno with no Linux counterpart named here.
    /// Reporting the wrong number is worse than reporting a generic I/O
    /// error, because the guest acts on it.
    pub const EIO: i32 = 5;
}

/// Highest errno that every supported host numbers identically.
const SHARED_ERRNO_MAX: i32 = 34;

/// Translate a host errno into the Linux errno a FUSE reply must carry.
///
/// illumos and Linux agree up to 34 and diverge above it: illumos
/// `ENOTSUP` is 48, which Linux reads as `ELNRNG`. A guest acts on the
/// number, so an untranslated reply makes it take an unrelated error
/// path -- an `execve` reading `security.capability`, for one, aborts
/// instead of treating the attribute as absent.
///
/// Takes and returns a positive errno.
pub fn to_linux_errno(host: i32) -> i32 {
    if (1..=SHARED_ERRNO_MAX).contains(&host) {
        return host;
    }
    // Matched by name, so each host maps from its own numbering. A `match`
    // cannot be used: on Linux several of these names share one value.
    let table: [(i32, i32); 20] = [
        (libc::EDEADLK, linux_errno::EDEADLK),
        (libc::ENAMETOOLONG, linux_errno::ENAMETOOLONG),
        (libc::ENOLCK, linux_errno::ENOLCK),
        (libc::ENOSYS, linux_errno::ENOSYS),
        (libc::ENOTEMPTY, linux_errno::ENOTEMPTY),
        (libc::ELOOP, linux_errno::ELOOP),
        (libc::ENOMSG, linux_errno::ENOMSG),
        (libc::EIDRM, linux_errno::EIDRM),
        (libc::ENODATA, linux_errno::ENODATA),
        (libc::ENOLINK, linux_errno::ENOLINK),
        (libc::EPROTO, linux_errno::EPROTO),
        (libc::EMULTIHOP, linux_errno::EMULTIHOP),
        (libc::EBADMSG, linux_errno::EBADMSG),
        (libc::EOVERFLOW, linux_errno::EOVERFLOW),
        (libc::EILSEQ, linux_errno::EILSEQ),
        (libc::ENOTSUP, linux_errno::EOPNOTSUPP),
        (libc::EOPNOTSUPP, linux_errno::EOPNOTSUPP),
        (libc::ESTALE, linux_errno::ESTALE),
        (libc::EDQUOT, linux_errno::EDQUOT),
        (libc::ECANCELED, linux_errno::ECANCELED),
    ];
    for (host_val, linux_val) in table {
        if host_val == host {
            return linux_val;
        }
    }
    linux_errno::EIO
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this guards: illumos ENOTSUP is 48, which a Linux guest
    /// reads as ELNRNG. An execve reading `security.capability` then
    /// aborts instead of treating the attribute as absent.
    #[test]
    fn errno_translation_uses_linux_numbering() {
        assert_eq!(to_linux_errno(libc::ENOTSUP), 95);
        assert_eq!(to_linux_errno(libc::EOPNOTSUPP), 95);
        assert_eq!(to_linux_errno(libc::ESTALE), 116);
        assert_eq!(to_linux_errno(libc::ENOTEMPTY), 39);
        assert_eq!(to_linux_errno(libc::ENAMETOOLONG), 36);
        assert_eq!(to_linux_errno(libc::ENOSYS), 38);
        assert_eq!(to_linux_errno(libc::ELOOP), 40);
    }

    #[test]
    fn classic_errno_range_passes_through() {
        // 1..=34 is the same on every host this builds for.
        for (host, name) in [
            (libc::ENOENT, "ENOENT"),
            (libc::EACCES, "EACCES"),
            (libc::EINVAL, "EINVAL"),
            (libc::EROFS, "EROFS"),
            (libc::EBADF, "EBADF"),
            (libc::EEXIST, "EEXIST"),
            (libc::ENOTDIR, "ENOTDIR"),
        ] {
            assert!(host <= 34, "{name} is not in the shared range");
            assert_eq!(to_linux_errno(host), host, "{name} was rewritten");
        }
    }

    /// An unrecognized host errno must not reach the guest unchanged:
    /// the guest would act on a number that means something else there.
    #[test]
    fn unknown_host_errno_becomes_eio() {
        assert_eq!(to_linux_errno(9999), 5);
    }

    #[test]
    fn header_layout_matches_linux() {
        assert_eq!(size_of::<FuseInHeader>(), 40);
        assert_eq!(size_of::<FuseOutHeader>(), 16);
        assert_eq!(size_of::<FuseAttr>(), 88);
        assert_eq!(size_of::<FuseEntryOut>(), 128);
        assert_eq!(size_of::<FuseAttrOut>(), 104);
        assert_eq!(size_of::<FuseInitOut>(), 64);
        assert_eq!(size_of::<FuseStatfsOut>(), 80);
    }

    #[test]
    fn dirent_size_padding() {
        assert_eq!(dirent_size(0), 24);
        assert_eq!(dirent_size(1), 32);
        assert_eq!(dirent_size(5), 32);
        assert_eq!(dirent_size(8), 32);
        assert_eq!(dirent_size(9), 40);
        assert_eq!(dirent_size(16), 40);
    }

    #[test]
    fn read_at_roundtrip() {
        let orig = FuseInHeader {
            len: 0x1234_5678,
            opcode: FUSE_LOOKUP,
            unique: 0xdead_beef_cafe_babe,
            nodeid: 1,
            uid: 501,
            gid: 20,
            pid: 42,
            padding: 0,
        };
        let bytes = bytes_of(&orig);
        let back: FuseInHeader = read_at(&bytes).unwrap();
        assert_eq!(back.len, orig.len);
        assert_eq!(back.opcode, orig.opcode);
        assert_eq!(back.unique, orig.unique);
        assert_eq!(back.nodeid, orig.nodeid);
        assert_eq!(back.uid, orig.uid);
        assert_eq!(back.gid, orig.gid);
        assert_eq!(back.pid, orig.pid);
    }

    // The literal Linux numbers, not `libc::O_*`: on illumos those are
    // different values, and that difference is the bug this guards.
    #[test]
    fn linux_open_flags_translate_to_host_flags() {
        let f = OpenFlags::from_linux(0o1 | 0o100 | 0o200).expect("known");
        assert_eq!(f.accmode, libc::O_WRONLY);
        assert!(f.creat && f.excl);
        assert!(!f.append && !f.trunc);

        let f = OpenFlags::from_linux(0o2 | 0o2000 | 0o1000).expect("known");
        assert_eq!(f.accmode, libc::O_RDWR);
        assert!(f.append && f.trunc);
        assert_eq!(
            f.host_io_flags(),
            libc::O_RDWR | libc::O_APPEND | libc::O_TRUNC
        );

        let f = OpenFlags::from_linux(0o200000 | 0o400000).expect("known");
        assert!(f.directory && f.nofollow);
        assert_eq!(f.host_io_flags(), libc::O_RDONLY);

        // O_SYNC carries the O_DSYNC bit on Linux; both are set then.
        let f = OpenFlags::from_linux(0o4010000).expect("known");
        assert!(f.sync && f.dsync);
    }

    #[test]
    fn ignored_linux_open_flags_are_accepted() {
        let f = OpenFlags::from_linux(
            0o100000 | 0o1000000 | 0o40000 | 0o400 | 0o2000000,
        )
        .expect("known");
        assert_eq!(f.host_io_flags(), libc::O_RDONLY);
    }

    #[test]
    fn unknown_linux_open_flags_are_refused() {
        // O_PATH, O_TMPFILE's own bit, and every bit above them.
        for bits in [0o10000000u32, 0o20000000, 1 << 31, 0o3] {
            assert_eq!(OpenFlags::from_linux(bits), Err(bits), "{bits:#o}");
        }
    }

    #[test]
    fn a_read_open_with_o_trunc_mutates() {
        assert!(OpenFlags::from_linux(0o1000).expect("known").mutates());
        assert!(!OpenFlags::from_linux(0).expect("known").mutates());
        assert!(OpenFlags::from_linux(0o100).expect("known").mutates());
    }

    #[test]
    fn read_at_rejects_short() {
        let h: Option<FuseInHeader> = read_at(&[0u8; 10]);
        assert!(h.is_none());
    }
}
