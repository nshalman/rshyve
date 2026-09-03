// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FUSE request dispatcher.
//!
//! Requests arrive as the `ChainBuf` list of one descriptor chain. We
//! build a `ChainReader` over the readable segments, pull the
//! `fuse_in_header`, read the remaining body, dispatch by opcode, and emit
//! a `fuse_out_header` + body through a `ChainWriter` over the writable
//! segments.
//!
//! `handle` runs on the FUSE worker thread, never on a vCPU thread: the
//! readers and writers build `SubMapping`s, which are `!Send`, so they
//! must be constructed where they are used.

use std::io::Read;
use std::io::Write;
use std::sync::Arc;

use vmm_core::mem::PhysMap;

use super::fuse::{self, bytes_of, push_val, read_at};
use super::passthrough::{Passthrough, PtError};
use crate::chain_io::{ChainReader, ChainWriter};
use crate::queue::ChainBuf;

/// Upper bound on the size of a request body we are willing to read from the
/// guest before processing. Caps individual FUSE messages at 1 MiB.
pub const MAX_MSG_SIZE: usize = 1024 * 1024;

/// Per-server limits exposed to the guest in FUSE_INIT_OUT.
pub const INIT_MAX_WRITE: u32 = 512 * 1024;
pub const INIT_MAX_BACKGROUND: u16 = 64;
pub const INIT_CONGESTION: u16 = 48;
pub const INIT_TIME_GRAN: u32 = 1;

pub struct FuseServer {
    passthrough: Arc<Passthrough>,
}

/// The "shape" of a handler's reply.
enum Reply {
    /// Success with an empty body (header only).
    Empty,
    /// Success with an opaque body.
    Body(Vec<u8>),
    /// Error (`-errno`, written into fuse_out_header.error).
    Error(i32),
    /// No reply expected (FORGET / BATCH_FORGET).
    None,
}

impl FuseServer {
    pub fn new(passthrough: Arc<Passthrough>) -> Self {
        Self { passthrough }
    }

    /// Borrow the passthrough backend, for reset and teardown.
    pub fn passthrough(&self) -> &Arc<Passthrough> {
        &self.passthrough
    }

    /// Process a single descriptor chain.
    ///
    /// Returns the number of bytes written into the chain's writable
    /// window, which the caller puts in the used-ring `len` field.
    pub fn handle(&self, bufs: &[ChainBuf], physmap: &PhysMap) -> u32 {
        // The two views cover disjoint segments of the same chain: the
        // reader skips writable segments and the writer skips readable
        // ones, so a guest that interleaves them cannot cross the halves.
        let mut reader = ChainReader::new(bufs, physmap);
        let mut writer = ChainWriter::new(bufs, physmap);

        // Header.
        let mut hdr_buf = [0u8; fuse::FUSE_IN_HEADER_SIZE];
        if reader.read_exact(&mut hdr_buf).is_err() {
            return 0;
        }
        let header: fuse::FuseInHeader = match read_at(&hdr_buf) {
            Some(h) => h,
            None => return 0,
        };

        // Validate header len.
        let total_len = header.len as usize;
        let limits = fuse::FUSE_IN_HEADER_SIZE..=MAX_MSG_SIZE;
        if !limits.contains(&total_len) {
            return write_reply(
                &mut writer,
                header.unique,
                Reply::Error(-libc::EINVAL),
            );
        }

        // Body.
        let body_len = total_len - fuse::FUSE_IN_HEADER_SIZE;
        let mut body = vec![0u8; body_len];
        if body_len > 0 && reader.read_exact(&mut body).is_err() {
            return write_reply(
                &mut writer,
                header.unique,
                Reply::Error(-libc::EINVAL),
            );
        }

        let reply = self.dispatch(&header, &body);
        write_reply(&mut writer, header.unique, reply)
    }

    fn dispatch(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        match header.opcode {
            fuse::FUSE_INIT => self.op_init(body),
            fuse::FUSE_DESTROY => Reply::Empty,
            fuse::FUSE_LOOKUP => self.op_lookup(header, body),
            fuse::FUSE_FORGET => {
                if let Some(f) = read_at::<fuse::FuseForgetIn>(body) {
                    self.passthrough.forget(header.nodeid, f.nlookup);
                } else {
                    self.passthrough.forget(header.nodeid, 1);
                }
                Reply::None
            }
            fuse::FUSE_BATCH_FORGET => {
                self.op_batch_forget(body);
                Reply::None
            }
            fuse::FUSE_GETATTR => self.op_getattr(header),
            fuse::FUSE_STATFS => self.op_statfs(header),
            fuse::FUSE_ACCESS => self.op_access(header, body),
            fuse::FUSE_READLINK => self.op_readlink(header),
            fuse::FUSE_OPEN => self.op_open(header, body),
            fuse::FUSE_RELEASE => self.op_release(body),
            fuse::FUSE_READ => self.op_read(body),
            fuse::FUSE_OPENDIR => self.op_opendir(header),
            fuse::FUSE_RELEASEDIR => self.op_releasedir(body),
            fuse::FUSE_READDIR => self.op_readdir(header, body, false),
            fuse::FUSE_READDIRPLUS => self.op_readdir(header, body, true),
            fuse::FUSE_FLUSH => Reply::Empty,
            fuse::FUSE_FSYNC | fuse::FUSE_FSYNCDIR => self.op_fsync(body),
            fuse::FUSE_GETXATTR | fuse::FUSE_LISTXATTR => {
                Reply::Error(-libc::ENOTSUP)
            }
            fuse::FUSE_WRITE => self.op_write(body),
            fuse::FUSE_CREATE => self.op_create(header, body),
            fuse::FUSE_MKDIR => self.op_mkdir(header, body),
            fuse::FUSE_MKNOD => self.op_mknod(header, body),
            fuse::FUSE_UNLINK => self.op_unlink(header, body),
            fuse::FUSE_RMDIR => self.op_rmdir(header, body),
            fuse::FUSE_RENAME => self.op_rename(header, body, false),
            fuse::FUSE_RENAME2 => self.op_rename(header, body, true),
            fuse::FUSE_SETATTR => self.op_setattr(header, body),
            fuse::FUSE_SYMLINK => self.op_symlink(header, body),
            fuse::FUSE_LINK => self.op_link(header, body),
            _ => Reply::Error(-libc::ENOSYS),
        }
    }

    // -------------------------------------------------------------------
    // Op handlers. Each returns a Reply.
    // -------------------------------------------------------------------

    fn op_init(&self, body: &[u8]) -> Reply {
        let init_in: fuse::FuseInitIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        if init_in.major < 7 {
            return Reply::Error(-libc::EPROTO);
        }
        let minor = init_in.minor.min(fuse::FUSE_KERNEL_MINOR_VERSION);
        let flags = fuse::FUSE_BIG_WRITES
            | fuse::FUSE_ASYNC_READ
            | fuse::FUSE_DO_READDIRPLUS
            | fuse::FUSE_READDIRPLUS_AUTO
            | fuse::FUSE_ATOMIC_O_TRUNC
            | fuse::FUSE_EXPORT_SUPPORT
            | fuse::FUSE_MAX_PAGES
            | fuse::FUSE_CACHE_SYMLINKS
            | fuse::FUSE_SUBMOUNTS;
        let out = fuse::FuseInitOut {
            major: fuse::FUSE_KERNEL_VERSION,
            minor,
            max_readahead: init_in.max_readahead,
            flags: flags & init_in.flags,
            max_background: INIT_MAX_BACKGROUND,
            congestion_threshold: INIT_CONGESTION,
            max_write: INIT_MAX_WRITE,
            time_gran: INIT_TIME_GRAN,
            max_pages: (INIT_MAX_WRITE / 4096) as u16,
            map_alignment: 0,
            unused: [0; 8],
        };
        Reply::Body(bytes_of(&out))
    }

    fn op_lookup(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let name = match extract_name(body) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.lookup(header.nodeid, &name) {
            Ok(entry) => Reply::Body(bytes_of(&entry)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_batch_forget(&self, body: &[u8]) {
        if body.len() < 8 {
            return;
        }
        let count = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
        let mut off = 8;
        for _ in 0..count {
            if off + 16 > body.len() {
                break;
            }
            let nodeid =
                u64::from_le_bytes(body[off..off + 8].try_into().unwrap());
            let nlookup =
                u64::from_le_bytes(body[off + 8..off + 16].try_into().unwrap());
            self.passthrough.forget(nodeid, nlookup);
            off += 16;
        }
    }

    fn op_getattr(&self, header: &fuse::FuseInHeader) -> Reply {
        match self.passthrough.getattr(header.nodeid) {
            Ok(attr) => Reply::Body(bytes_of(&fuse::FuseAttrOut {
                attr_valid: 1,
                attr_valid_nsec: 0,
                dummy: 0,
                attr,
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_statfs(&self, header: &fuse::FuseInHeader) -> Reply {
        match self.passthrough.statfs(header.nodeid) {
            Ok(s) => Reply::Body(bytes_of(&s)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_access(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let acc: fuse::FuseAccessIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.access(header.nodeid, acc.mask) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_readlink(&self, header: &fuse::FuseInHeader) -> Reply {
        match self.passthrough.readlink(header.nodeid) {
            Ok(target) => Reply::Body(target),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_open(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let oi: fuse::FuseOpenIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        // O_TRUNC truncates even alongside O_RDONLY on illumos, so the
        // access mode alone does not say whether an open mutates.
        let mutating = oi.flags & libc::O_ACCMODE as u32 != 0
            || oi.flags & libc::O_TRUNC as u32 != 0;
        if self.passthrough.is_read_only() && mutating {
            return Reply::Error(-libc::EROFS);
        }
        match self.passthrough.open(header.nodeid, oi.flags) {
            Ok(fh) => Reply::Body(bytes_of(&fuse::FuseOpenOut {
                fh,
                open_flags: fuse::FOPEN_KEEP_CACHE,
                padding: 0,
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_release(&self, body: &[u8]) -> Reply {
        let r: fuse::FuseReleaseIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.release(r.fh) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_read(&self, body: &[u8]) -> Reply {
        let rd: fuse::FuseReadIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        if rd.size as usize > MAX_MSG_SIZE {
            return Reply::Error(-libc::EINVAL);
        }
        match self.passthrough.read(rd.fh, rd.offset, rd.size) {
            Ok(data) => Reply::Body(data),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_opendir(&self, header: &fuse::FuseInHeader) -> Reply {
        match self.passthrough.opendir(header.nodeid) {
            Ok(fh) => Reply::Body(bytes_of(&fuse::FuseOpenOut {
                fh,
                open_flags: fuse::FOPEN_CACHE_DIR,
                padding: 0,
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_releasedir(&self, body: &[u8]) -> Reply {
        let r: fuse::FuseReleaseIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.releasedir(r.fh) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_readdir(
        &self,
        header: &fuse::FuseInHeader,
        body: &[u8],
        with_plus: bool,
    ) -> Reply {
        let rd: fuse::FuseReadIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let max = (rd.size as usize).min(MAX_MSG_SIZE);
        let mut out: Vec<u8> = Vec::with_capacity(max.min(64 * 1024));
        let mut saw_err: Option<PtError> = None;
        let pt = &self.passthrough;
        let res = pt.readdir_each(rd.fh, rd.offset, |ino, name, dt, next| {
            let name_bytes = name.to_bytes();
            let namelen = name_bytes.len();
            let plus_prefix = if with_plus {
                size_of::<fuse::FuseEntryOut>()
            } else {
                0
            };
            let needed = plus_prefix + fuse::dirent_size(namelen);
            if out.len() + needed > max {
                return false;
            }
            if with_plus {
                let entry = match pt.lookup(header.nodeid, name) {
                    Ok(e) => e,
                    Err(e) => {
                        saw_err = Some(e);
                        return false;
                    }
                };
                push_val(&mut out, &entry);
            }
            let d = fuse::FuseDirent {
                ino,
                off: next,
                namelen: namelen as u32,
                typ: dt,
            };
            push_val(&mut out, &d);
            out.extend_from_slice(name_bytes);
            let pad =
                fuse::dirent_size(namelen) - fuse::FUSE_DIRENT_SIZE - namelen;
            out.extend(std::iter::repeat_n(0u8, pad));
            true
        });
        if let Err(e) = res {
            return Reply::Error(-e.to_errno());
        }
        if let Some(e) = saw_err {
            return Reply::Error(-e.to_errno());
        }
        Reply::Body(out)
    }

    // -------------------------------------------------------------------
    // Write-path handlers
    // -------------------------------------------------------------------

    fn op_write(&self, body: &[u8]) -> Reply {
        let write_hdr_size = size_of::<fuse::FuseWriteIn>();
        if body.len() < write_hdr_size {
            return Reply::Error(-libc::EINVAL);
        }
        let w: fuse::FuseWriteIn = match read_at(&body[..write_hdr_size]) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let data_len = w.size as usize;
        if data_len > MAX_MSG_SIZE || body.len() < write_hdr_size + data_len {
            return Reply::Error(-libc::EINVAL);
        }
        let data = &body[write_hdr_size..write_hdr_size + data_len];
        match self.passthrough.write(w.fh, w.offset, data) {
            Ok(n) => Reply::Body(bytes_of(&fuse::FuseWriteOut {
                size: n,
                padding: 0,
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_create(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let ci: fuse::FuseCreateIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let name_off = size_of::<fuse::FuseCreateIn>();
        let name = match extract_name(&body[name_off..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.create(
            header.nodeid,
            &name,
            ci.flags,
            ci.mode,
            ci.umask,
        ) {
            Ok((entry, fh, _)) => Reply::Body(bytes_of(&fuse::FuseCreateOut {
                entry,
                open: fuse::FuseOpenOut {
                    fh,
                    open_flags: fuse::FOPEN_KEEP_CACHE,
                    padding: 0,
                },
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_mkdir(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let mi: fuse::FuseMkdirIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let name_off = size_of::<fuse::FuseMkdirIn>();
        let name = match extract_name(&body[name_off..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self
            .passthrough
            .mkdir(header.nodeid, &name, mi.mode, mi.umask)
        {
            Ok(entry) => Reply::Body(bytes_of(&entry)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_mknod(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        if body.len() < 16 {
            return Reply::Error(-libc::EINVAL);
        }
        let mode = u32::from_le_bytes(body[0..4].try_into().unwrap());
        let rdev = u32::from_le_bytes(body[4..8].try_into().unwrap());
        let umask = u32::from_le_bytes(body[8..12].try_into().unwrap());
        let name = match extract_name(&body[16..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self
            .passthrough
            .mknod(header.nodeid, &name, mode, rdev, umask)
        {
            Ok(entry) => Reply::Body(bytes_of(&entry)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_unlink(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let name = match extract_name(body) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.unlink(header.nodeid, &name) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_rmdir(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let name = match extract_name(body) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.rmdir(header.nodeid, &name) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_rename(
        &self,
        header: &fuse::FuseInHeader,
        body: &[u8],
        is_rename2: bool,
    ) -> Reply {
        let (newdir, flags, names_off) = if is_rename2 {
            if body.len() < 16 {
                return Reply::Error(-libc::EINVAL);
            }
            let newdir = u64::from_le_bytes(body[0..8].try_into().unwrap());
            let flags = u32::from_le_bytes(body[8..12].try_into().unwrap());
            (newdir, flags, 16)
        } else {
            if body.len() < 8 {
                return Reply::Error(-libc::EINVAL);
            }
            let newdir = u64::from_le_bytes(body[0..8].try_into().unwrap());
            (newdir, 0u32, 8)
        };
        if flags != 0 {
            return Reply::Error(-libc::ENOSYS);
        }
        let old_name = match extract_name(&body[names_off..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        let old_len = old_name.as_bytes_with_nul().len();
        let new_name = match extract_name(&body[names_off + old_len..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.rename(
            header.nodeid,
            &old_name,
            newdir,
            &new_name,
            flags,
        ) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_setattr(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let sa: fuse::FuseSetattrIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let fh = if sa.valid & fuse::FATTR_FH != 0 {
            Some(sa.fh)
        } else {
            None
        };
        let res = self.passthrough.setattr(
            header.nodeid,
            sa.valid,
            fh,
            sa.size,
            sa.mode,
            sa.uid,
            sa.gid,
            (
                sa.atime,
                sa.atimensec,
                sa.valid & fuse::FATTR_ATIME_NOW != 0,
            ),
            (
                sa.mtime,
                sa.mtimensec,
                sa.valid & fuse::FATTR_MTIME_NOW != 0,
            ),
        );
        match res {
            Ok(attr) => Reply::Body(bytes_of(&fuse::FuseAttrOut {
                attr_valid: 1,
                attr_valid_nsec: 0,
                dummy: 0,
                attr,
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_symlink(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let name = match extract_name(body) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        let name_len = name.as_bytes_with_nul().len();
        let target = match extract_name(&body[name_len..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.symlink(header.nodeid, &name, &target) {
            Ok(entry) => Reply::Body(bytes_of(&entry)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_link(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        if body.len() < 8 {
            return Reply::Error(-libc::EINVAL);
        }
        let oldnodeid = u64::from_le_bytes(body[0..8].try_into().unwrap());
        let new_name = match extract_name(&body[8..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.link(oldnodeid, header.nodeid, &new_name) {
            Ok(entry) => Reply::Body(bytes_of(&entry)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_fsync(&self, body: &[u8]) -> Reply {
        let f: fuse::FuseFsyncIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let datasync = f.fsync_flags & 1 != 0;
        match self.passthrough.fsync(f.fh, datasync) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

fn write_reply<W: Write>(writer: &mut W, unique: u64, reply: Reply) -> u32 {
    match reply {
        Reply::None => 0,
        Reply::Empty => write_header(writer, unique, 0, &[]),
        Reply::Body(body) => write_header(writer, unique, 0, &body),
        // The only point where an errno reaches the guest, so it is the
        // only place the host-to-Linux translation has to happen.
        Reply::Error(err) => {
            let linux = if err < 0 {
                -fuse::to_linux_errno(-err)
            } else {
                err
            };
            write_header(writer, unique, linux, &[])
        }
    }
}

fn write_header<W: Write>(
    writer: &mut W,
    unique: u64,
    err: i32,
    body: &[u8],
) -> u32 {
    let total = fuse::FUSE_OUT_HEADER_SIZE + body.len();
    let hdr = fuse::FuseOutHeader {
        len: total as u32,
        error: err,
        unique,
    };
    let hdr_bytes = bytes_of(&hdr);
    let mut written = 0usize;
    match writer.write_all(&hdr_bytes) {
        Ok(()) => written += hdr_bytes.len(),
        Err(_) => return 0,
    }
    // On a short-body write the writable window was too small. Still ack
    // the header that already landed, so the guest sees a reply.
    if !body.is_empty() && writer.write_all(body).is_ok() {
        written += body.len();
    }
    written as u32
}

/// Interpret a zero-terminated name at the start of `buf`.
fn extract_name(buf: &[u8]) -> Option<std::ffi::CString> {
    let end = buf.iter().position(|b| *b == 0)?;
    if end == 0 {
        return None;
    }
    std::ffi::CString::new(&buf[..end]).ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const GPA: u64 = 0x20_0000;
    const LEN: usize = 0x4000;
    const REQ: u64 = GPA;
    const RESP: u64 = GPA + 0x1000;

    fn anon() -> PhysMap {
        PhysMap::new_anon(GPA, LEN).expect("anon region")
    }

    fn server() -> FuseServer {
        let pt =
            Passthrough::new(&std::env::temp_dir(), true).expect("open temp");
        FuseServer::new(Arc::new(pt))
    }

    fn seed(physmap: &PhysMap, gpa: u64, data: &[u8]) {
        physmap
            .lookup(gpa, data.len())
            .expect("mapped")
            .write_bytes(data)
            .expect("seed guest memory");
    }

    fn fetch(physmap: &PhysMap, gpa: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        physmap
            .lookup(gpa, len)
            .expect("mapped")
            .read_bytes(&mut out)
            .expect("read reply");
        out
    }

    /// Build a FUSE_INIT request: 40-byte header + 16-byte fuse_init_in.
    fn init_request() -> Vec<u8> {
        let body = fuse::FuseInitIn {
            major: 7,
            minor: 33,
            max_readahead: 0x2_0000,
            flags: 0,
        };
        let hdr = fuse::FuseInHeader {
            len: (fuse::FUSE_IN_HEADER_SIZE + 16) as u32,
            opcode: fuse::FUSE_INIT,
            unique: 0x4242,
            nodeid: 0,
            uid: 0,
            gid: 0,
            pid: 0,
            padding: 0,
        };
        let mut buf = bytes_of(&hdr);
        push_val(&mut buf, &body);
        buf
    }

    #[test]
    fn init_over_a_chain_returns_a_success_header() {
        let physmap = anon();
        let req = init_request();
        seed(&physmap, REQ, &req);

        let bufs = vec![
            ChainBuf::Readable {
                addr: REQ,
                len: req.len() as u32,
            },
            ChainBuf::Writable {
                addr: RESP,
                len: 256,
            },
        ];

        let written = server().handle(&bufs, &physmap);
        assert_eq!(
            written as usize,
            fuse::FUSE_OUT_HEADER_SIZE + size_of::<fuse::FuseInitOut>()
        );

        let out = fetch(&physmap, RESP, written as usize);
        let hdr: fuse::FuseOutHeader = read_at(&out).expect("out header");
        assert_eq!(hdr.error, 0);
        assert_eq!(hdr.unique, 0x4242);
        assert_eq!(hdr.len, written);

        let init: fuse::FuseInitOut =
            read_at(&out[fuse::FUSE_OUT_HEADER_SIZE..]).expect("init out");
        assert_eq!(init.major, 7);
        assert_eq!(init.minor, 33);
        assert_eq!(init.max_write, INIT_MAX_WRITE);
    }

    // A guest can declare any length in the header. Anything past the
    // message cap must be refused before a body of that size is allocated.
    #[test]
    fn oversized_declared_len_is_rejected_with_einval() {
        let physmap = anon();
        let mut req = init_request();
        let bogus = (MAX_MSG_SIZE as u32) + 1;
        req[..4].copy_from_slice(&bogus.to_le_bytes());
        seed(&physmap, REQ, &req);

        let bufs = vec![
            ChainBuf::Readable {
                addr: REQ,
                len: req.len() as u32,
            },
            ChainBuf::Writable {
                addr: RESP,
                len: 256,
            },
        ];

        let written = server().handle(&bufs, &physmap);
        assert_eq!(written as usize, fuse::FUSE_OUT_HEADER_SIZE);

        let out = fetch(&physmap, RESP, written as usize);
        let hdr: fuse::FuseOutHeader = read_at(&out).expect("out header");
        assert_eq!(hdr.error, -libc::EINVAL);
    }

    #[test]
    fn a_chain_with_no_readable_segment_writes_nothing() {
        let physmap = anon();
        let bufs = vec![ChainBuf::Writable {
            addr: RESP,
            len: 256,
        }];
        assert_eq!(server().handle(&bufs, &physmap), 0);
    }

    // A writable window too small for the body still acks the header, so
    // the guest sees a reply instead of a stalled request.
    #[test]
    fn a_short_writable_window_still_acks_the_header() {
        let physmap = anon();
        let req = init_request();
        seed(&physmap, REQ, &req);

        let bufs = vec![
            ChainBuf::Readable {
                addr: REQ,
                len: req.len() as u32,
            },
            ChainBuf::Writable {
                addr: RESP,
                len: fuse::FUSE_OUT_HEADER_SIZE as u32,
            },
        ];

        let written = server().handle(&bufs, &physmap);
        assert_eq!(written as usize, fuse::FUSE_OUT_HEADER_SIZE);
    }

    #[test]
    fn extract_trims_nul() {
        let buf = b"foo\0bar\0";
        let c = extract_name(buf).unwrap();
        assert_eq!(c.as_bytes(), b"foo");
    }

    #[test]
    fn extract_rejects_empty() {
        assert!(extract_name(&[]).is_none());
        assert!(extract_name(b"\0").is_none());
    }

    // -------------------------------------------------------------------
    // Wire-level security properties
    //
    // `extract_name` does not validate: it stops at the first NUL and
    // returns whatever bytes precede it. The guard against a guest walking
    // out of the shared directory lives one layer down, in the passthrough
    // backend. These tests drive whole requests through `handle` so the
    // property is proved at the boundary the guest actually reaches, not
    // at the backend call the dispatcher happens to make today.
    // -------------------------------------------------------------------

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

    fn server_on(dir: &tempfile::TempDir, read_only: bool) -> FuseServer {
        let pt = Passthrough::new(dir.path(), read_only).expect("open dir");
        FuseServer::new(Arc::new(pt))
    }

    /// FUSE_MKDIR against the root node, carrying `name` verbatim.
    fn mkdir_request(name: &[u8]) -> Vec<u8> {
        let body = fuse::FuseMkdirIn {
            mode: 0o755,
            umask: 0,
        };
        let body_len = size_of::<fuse::FuseMkdirIn>() + name.len() + 1;
        let hdr = fuse::FuseInHeader {
            len: (fuse::FUSE_IN_HEADER_SIZE + body_len) as u32,
            opcode: fuse::FUSE_MKDIR,
            unique: 0x5150,
            nodeid: fuse::FUSE_ROOT_ID,
            uid: 0,
            gid: 0,
            pid: 0,
            padding: 0,
        };
        let mut buf = bytes_of(&hdr);
        push_val(&mut buf, &body);
        buf.extend_from_slice(name);
        buf.push(0);
        buf
    }

    /// Run one request through `handle` and return the reply's errno field.
    fn reply_error(srv: &FuseServer, req: &[u8]) -> i32 {
        let physmap = anon();
        seed(&physmap, REQ, req);
        let bufs = vec![
            ChainBuf::Readable {
                addr: REQ,
                len: req.len() as u32,
            },
            ChainBuf::Writable {
                addr: RESP,
                len: 512,
            },
        ];
        let written = srv.handle(&bufs, &physmap);
        assert!(written as usize >= fuse::FUSE_OUT_HEADER_SIZE);
        let out = fetch(&physmap, RESP, written as usize);
        let hdr: fuse::FuseOutHeader = read_at(&out).expect("out header");
        hdr.error
    }

    // A writable share must still refuse to leave its root. This is the
    // case that matters: read-only would mask the traversal behind EROFS.
    #[test]
    fn a_dotdot_component_is_refused_on_a_writable_share() {
        let dir = private_dir();
        let srv = server_on(&dir, false);
        assert_eq!(reply_error(&srv, &mkdir_request(b"..")), -libc::EINVAL);
        assert!(!dir.path().parent().unwrap().join("escaped").exists());
    }

    #[test]
    fn an_embedded_slash_is_refused_on_a_writable_share() {
        let dir = private_dir();
        let srv = server_on(&dir, false);
        assert_eq!(
            reply_error(&srv, &mkdir_request(b"sub/escaped")),
            -libc::EINVAL
        );
        assert_eq!(reply_error(&srv, &mkdir_request(b"/abs")), -libc::EINVAL);
    }

    // The name check runs before the read-only check, so a traversal name
    // reports EINVAL rather than EROFS even on a read-only share. Pinning
    // that order keeps a future edit from hiding one guard behind the other.
    #[test]
    fn the_name_check_precedes_the_read_only_check() {
        let dir = private_dir();
        let srv = server_on(&dir, true);
        assert_eq!(reply_error(&srv, &mkdir_request(b"..")), -libc::EINVAL);
    }

    #[test]
    fn a_mutating_op_is_refused_on_a_read_only_share() {
        let dir = private_dir();
        let srv = server_on(&dir, true);
        assert_eq!(
            reply_error(&srv, &mkdir_request(b"ordinary")),
            -libc::EROFS
        );
        assert!(!dir.path().join("ordinary").exists());
    }

    // The same request succeeds when the share is writable, so the two
    // tests above prove enforcement rather than a broken request shape.
    #[test]
    fn the_same_mkdir_succeeds_on_a_writable_share() {
        let dir = private_dir();
        let srv = server_on(&dir, false);
        assert_eq!(reply_error(&srv, &mkdir_request(b"ordinary")), 0);
        assert!(dir.path().join("ordinary").is_dir());
    }

    /// FUSE_OPEN of `nodeid`, carrying the guest's open flags verbatim.
    fn open_request(nodeid: u64, flags: u32) -> Vec<u8> {
        let body = fuse::FuseOpenIn { flags, unused: 0 };
        let hdr = fuse::FuseInHeader {
            len: (fuse::FUSE_IN_HEADER_SIZE + size_of::<fuse::FuseOpenIn>())
                as u32,
            opcode: fuse::FUSE_OPEN,
            unique: 0x6060,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            padding: 0,
        };
        let mut buf = bytes_of(&hdr);
        push_val(&mut buf, &body);
        buf
    }

    /// Seed a file with content and return its nodeid, via a real LOOKUP.
    fn seeded_file(dir: &tempfile::TempDir, srv: &FuseServer) -> u64 {
        std::fs::write(dir.path().join("payload"), b"KEEP THIS DATA")
            .expect("seed file");
        let pt = srv.passthrough();
        let name = std::ffi::CString::new("payload").expect("name");
        pt.lookup(fuse::FUSE_ROOT_ID, &name).expect("lookup").nodeid
    }

    // O_TRUNC destroys a file whatever the access mode claims: illumos
    // honors O_RDONLY | O_TRUNC and truncates. A guest that sends it at a
    // read-only share must be refused, or the share is not read-only.
    #[test]
    fn o_trunc_cannot_destroy_a_file_on_a_read_only_share() {
        let dir = private_dir();
        let srv = server_on(&dir, true);
        let nodeid = seeded_file(&dir, &srv);
        let flags = (libc::O_RDONLY | libc::O_TRUNC) as u32;

        assert_eq!(
            reply_error(&srv, &open_request(nodeid, flags)),
            -libc::EROFS
        );
        assert_eq!(
            std::fs::read(dir.path().join("payload")).expect("read back"),
            b"KEEP THIS DATA",
            "a read-only share truncated the host file"
        );
    }

    // A plain read open is still allowed, so the guard above rejects the
    // truncation rather than every open.
    //
    // This and the next test reach `reopen_fd`, which reopens through
    // /proc/self/fd. That path does not exist on a macOS dev host, so both
    // run only on the deployment target.
    #[cfg(target_os = "illumos")]
    #[test]
    fn a_plain_read_open_still_works_on_a_read_only_share() {
        let dir = private_dir();
        let srv = server_on(&dir, true);
        let nodeid = seeded_file(&dir, &srv);
        let flags = libc::O_RDONLY as u32;
        assert_eq!(reply_error(&srv, &open_request(nodeid, flags)), 0);
    }

    #[cfg(target_os = "illumos")]
    #[test]
    fn o_trunc_still_works_on_a_writable_share() {
        let dir = private_dir();
        let srv = server_on(&dir, false);
        let nodeid = seeded_file(&dir, &srv);
        let flags = (libc::O_WRONLY | libc::O_TRUNC) as u32;
        assert_eq!(reply_error(&srv, &open_request(nodeid, flags)), 0);
        assert!(std::fs::read(dir.path().join("payload"))
            .expect("read back")
            .is_empty());
    }
}
