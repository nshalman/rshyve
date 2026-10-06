// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The control socket.
//!
//! illumos cannot bound a blocking AF_UNIX send in time, so the socket is
//! non-blocking and every read and write polls up to a deadline. Every
//! read is a `recvmsg`, so a descriptor sent with any frame is taken and
//! closed, not left queued on the socket.

use std::io::{self, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

use manta_block_ring::control::{Message, HELLO_VERSION, MAX_FRAME};
use manta_block_ring::layout::MAX_QUEUES;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags,
    ReturnFlags, SendFlags, SocketAddrUnix, SocketFlags, SocketType,
};

// illumos has no MSG_CMSG_CLOEXEC; there each descriptor gets
// close-on-exec right after receipt.
#[cfg(not(any(target_os = "illumos", target_os = "solaris")))]
const RECV_FLAGS: RecvFlags = RecvFlags::CMSG_CLOEXEC;
#[cfg(any(target_os = "illumos", target_os = "solaris"))]
const RECV_FLAGS: RecvFlags = RecvFlags::empty();

/// Most descriptors any frame may carry: AttachOk with every queue.
const MAX_FDS: usize = 1 + 2 * MAX_QUEUES as usize;

pub(crate) struct Frame {
    pub(crate) msg: Message,
    pub(crate) fds: Vec<OwnedFd>,
}

pub(crate) struct Conn {
    s: UnixStream,
    buf: Vec<u8>,
    /// Descriptors received since the last whole frame.
    fds: Vec<OwnedFd>,
    next_id: u32,
    /// Frames after HelloAck go in the agreed version.
    version: u16,
}

impl Conn {
    /// A blocking connect can wait for ever on a listener whose backlog
    /// is full, so this one is bounded by `deadline`.
    pub(crate) fn connect(path: &Path, deadline: Instant) -> io::Result<Self> {
        let fd = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )?;
        match rustix::net::connect(&fd, &SocketAddrUnix::new(path)?) {
            Ok(()) => {}
            Err(Errno::INPROGRESS) => {
                wait(fd.as_fd(), PollFlags::OUT, deadline)?;
                rustix::net::sockopt::socket_error(&fd)??;
            }
            Err(e) => return Err(e.into()),
        }
        Self::new(UnixStream::from(fd))
    }

    pub(crate) fn new(s: UnixStream) -> io::Result<Self> {
        s.set_nonblocking(true)?;
        Ok(Self {
            s,
            buf: Vec::new(),
            fds: Vec::new(),
            next_id: 1,
            version: HELLO_VERSION,
        })
    }

    /// Every frame after this goes in `version`.
    pub(crate) fn set_version(&mut self, version: u16) {
        self.version = version;
    }

    /// Returns the frame id.
    pub(crate) fn send(
        &mut self,
        msg: &Message,
        deadline: Instant,
    ) -> io::Result<u32> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let frame = msg.encode_as(id, self.version);
        let mut rest = &frame[..];
        while !rest.is_empty() {
            // A closed peer is an error here, not a signal.
            match rustix::net::send(&self.s, rest, SendFlags::NOSIGNAL) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => rest = &rest[n..],
                Err(Errno::AGAIN) => self.wait(PollFlags::OUT, deadline)?,
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(id)
    }

    /// `Ok(None)` when the engine closed the socket between frames.
    pub(crate) fn recv(
        &mut self,
        deadline: Instant,
    ) -> io::Result<Option<Frame>> {
        loop {
            if let Some((msg, _, used)) =
                Message::decode_as(&self.buf, self.version).map_err(invalid)?
            {
                self.buf.drain(..used);
                let fds = std::mem::take(&mut self.fds);
                return Ok(Some(Frame { msg, fds }));
            }
            // A peer that keeps the socket readable never reaches `wait`.
            if Instant::now() >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
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

    pub(crate) fn try_recv(&mut self) -> io::Result<Polled> {
        loop {
            if let Some((msg, _, used)) =
                Message::decode_as(&self.buf, self.version).map_err(invalid)?
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

    /// True when [`Self::try_recv`] would return without reading: a whole
    /// frame is buffered, or bytes that will not decode.
    pub(crate) fn frame_buffered(&self) -> bool {
        !matches!(Message::decode_as(&self.buf, self.version), Ok(None))
    }

    /// A timeout is not an error here.
    pub(crate) fn readable(&self, deadline: Instant) -> io::Result<()> {
        match self.wait(PollFlags::IN, deadline) {
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(()),
            other => other,
        }
    }

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
        wait(self.s.as_fd(), flags, deadline)
    }
}

fn wait(
    fd: BorrowedFd<'_>,
    flags: PollFlags,
    deadline: Instant,
) -> io::Result<()> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::ErrorKind::TimedOut.into());
    }
    let ts = Timespec {
        tv_sec: left.as_secs() as _,
        tv_nsec: left.subsec_nanos() as _,
    };
    let mut fds = [PollFd::from_borrowed_fd(fd, flags)];
    match rustix::event::poll(&mut fds, Some(&ts)) {
        Ok(0) => Err(io::ErrorKind::TimedOut.into()),
        Ok(_) | Err(Errno::INTR) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub(crate) enum Polled {
    Frame(Frame),
    Empty,
    Closed,
}

pub(crate) fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

enum Read {
    Bytes,
    WouldBlock,
    Eof,
}
