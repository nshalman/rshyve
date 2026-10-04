// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The control socket (spec 37 §2, §6).
//!
//! illumos cannot bound a blocking AF_UNIX send in time, so the socket is
//! non-blocking and every read and write waits with `poll` up to a
//! deadline. Every read is a `recvmsg`, so a descriptor the engine sends
//! with any frame is taken, and closed unless the frame is AttachOk.

use std::io::{self, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

use manta_block_ring::control::{Message, MAX_FRAME};
use manta_block_ring::layout::MAX_QUEUES;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendFlags,
};

// illumos has no MSG_CMSG_CLOEXEC; there each descriptor gets
// close-on-exec right after receipt.
#[cfg(not(any(target_os = "illumos", target_os = "solaris")))]
const RECV_FLAGS: RecvFlags = RecvFlags::CMSG_CLOEXEC;
#[cfg(any(target_os = "illumos", target_os = "solaris"))]
const RECV_FLAGS: RecvFlags = RecvFlags::empty();

/// Most descriptors any frame may carry: AttachOk with every queue.
const MAX_FDS: usize = 1 + 2 * MAX_QUEUES as usize;

/// One frame and the descriptors that came with it.
pub(crate) struct Frame {
    pub(crate) msg: Message,
    pub(crate) fds: Vec<OwnedFd>,
}

/// A connection to the engine.
pub(crate) struct Conn {
    s: UnixStream,
    buf: Vec<u8>,
    /// Descriptors received since the last whole frame.
    fds: Vec<OwnedFd>,
    next_id: u32,
}

impl Conn {
    pub(crate) fn connect(path: &Path) -> io::Result<Self> {
        let s = UnixStream::connect(path)?;
        s.set_nonblocking(true)?;
        Ok(Self {
            s,
            buf: Vec::new(),
            fds: Vec::new(),
            next_id: 1,
        })
    }

    /// Sends `msg` before `deadline`. Returns its id.
    pub(crate) fn send(
        &mut self,
        msg: &Message,
        deadline: Instant,
    ) -> io::Result<u32> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let frame = msg.encode(id);
        let mut rest = &frame[..];
        while !rest.is_empty() {
            match rustix::net::send(&self.s, rest, SendFlags::empty()) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => rest = &rest[n..],
                Err(Errno::AGAIN) => self.wait(PollFlags::OUT, deadline)?,
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(id)
    }

    /// The next frame, waiting until `deadline`. `Ok(None)` when the
    /// engine closed the socket between frames.
    pub(crate) fn recv(
        &mut self,
        deadline: Instant,
    ) -> io::Result<Option<Frame>> {
        loop {
            if let Some((msg, _, used)) =
                Message::decode(&self.buf).map_err(invalid)?
            {
                self.buf.drain(..used);
                let fds = std::mem::take(&mut self.fds);
                return Ok(Some(Frame { msg, fds }));
            }
            match self.read_some()? {
                Read::Bytes => {}
                Read::WouldBlock => self.wait(PollFlags::IN, deadline)?,
                Read::Eof if self.buf.is_empty() && self.fds.is_empty() => {
                    return Ok(None)
                }
                Read::Eof => return Err(io::ErrorKind::UnexpectedEof.into()),
            }
        }
    }

    /// As [`Conn::recv`], but `Ok(None)` also when nothing is there now.
    pub(crate) fn try_recv(&mut self) -> io::Result<Polled> {
        loop {
            if let Some((msg, _, used)) =
                Message::decode(&self.buf).map_err(invalid)?
            {
                self.buf.drain(..used);
                let fds = std::mem::take(&mut self.fds);
                return Ok(Polled::Frame(Frame { msg, fds }));
            }
            match self.read_some()? {
                Read::Bytes => {}
                Read::WouldBlock => return Ok(Polled::Empty),
                Read::Eof => return Ok(Polled::Closed),
            }
        }
    }

    /// Waits until the socket is readable or `deadline` passes.
    pub(crate) fn readable(&self, deadline: Instant) -> io::Result<()> {
        match self.wait(PollFlags::IN, deadline) {
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(()),
            other => other,
        }
    }

    /// One `recvmsg`, keeping what it brought.
    fn read_some(&mut self) -> io::Result<Read> {
        let mut chunk = [0u8; MAX_FRAME];
        let mut space =
            [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
        loop {
            let mut anc = RecvAncillaryBuffer::new(&mut space);
            let mut iov = [IoSliceMut::new(&mut chunk)];
            match rustix::net::recvmsg(
                self.s.as_fd(),
                &mut iov,
                &mut anc,
                RECV_FLAGS,
            ) {
                Ok(r) => {
                    // Take the descriptors first, so they close on any
                    // error below.
                    for m in anc.drain() {
                        if let RecvAncillaryMessage::ScmRights(it) = m {
                            for fd in it {
                                rustix::io::fcntl_setfd(
                                    &fd,
                                    rustix::io::FdFlags::CLOEXEC,
                                )?;
                                self.fds.push(fd);
                            }
                        }
                    }
                    if r.flags.contains(ReturnFlags::CTRUNC) {
                        return Err(invalid("descriptors truncated"));
                    }
                    if self.fds.len() > MAX_FDS {
                        return Err(invalid("too many descriptors"));
                    }
                    if r.bytes == 0 {
                        return Ok(Read::Eof);
                    }
                    self.buf.extend_from_slice(&chunk[..r.bytes]);
                    return Ok(Read::Bytes);
                }
                Err(Errno::AGAIN) => return Ok(Read::WouldBlock),
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn wait(&self, flags: PollFlags, deadline: Instant) -> io::Result<()> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let ts = Timespec {
            tv_sec: left.as_secs() as _,
            tv_nsec: left.subsec_nanos() as _,
        };
        let mut fds = [PollFd::from_borrowed_fd(self.s.as_fd(), flags)];
        match rustix::event::poll(&mut fds, Some(&ts)) {
            Ok(0) => Err(io::ErrorKind::TimedOut.into()),
            Ok(_) | Err(Errno::INTR) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// What [`Conn::try_recv`] found.
pub(crate) enum Polled {
    Frame(Frame),
    Empty,
    Closed,
}

pub(crate) fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// What one `recvmsg` did.
enum Read {
    Bytes,
    WouldBlock,
    Eof,
}
