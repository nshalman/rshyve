// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Connects a stop can cut short on the calling thread.
//!
//! A blocking connect cannot be interrupted, and one left on a helper
//! thread can still land later and wake a peer that has no one to talk
//! to. These connect non-blocking and close the socket on a stop.

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustix::net::{
    addr::SocketAddrArg, AddressFamily, SocketAddrUnix, SocketFlags, SocketType,
};

use crate::codec::MigrateError;
use crate::wait::CANCEL_POLL;

/// Connect to a Unix socket until `cancelled`. The outer error is the
/// cancel; the inner one is the connect's own.
pub fn unix(
    path: &Path,
    cancelled: impl Fn() -> bool,
) -> Result<io::Result<UnixStream>, MigrateError> {
    let addr = match SocketAddrUnix::new(path) {
        Ok(addr) => addr,
        Err(e) => return Ok(Err(e.into())),
    };
    Ok(connect(AddressFamily::UNIX, &addr, None, &cancelled)?
        .map(UnixStream::from))
}

/// As [`unix`], giving up at `deadline` as well.
pub fn unix_until(
    path: &Path,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<io::Result<UnixStream>, MigrateError> {
    let addr = match SocketAddrUnix::new(path) {
        Ok(addr) => addr,
        Err(e) => return Ok(Err(e.into())),
    };
    Ok(
        connect(AddressFamily::UNIX, &addr, Some(deadline), &cancelled)?
            .map(UnixStream::from),
    )
}

/// Connect to `addr` until `cancelled` or `timeout`.
pub fn tcp(
    addr: SocketAddr,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<io::Result<TcpStream>, MigrateError> {
    let family = match addr {
        SocketAddr::V4(_) => AddressFamily::INET,
        SocketAddr::V6(_) => AddressFamily::INET6,
    };
    let deadline = Instant::now().checked_add(timeout);
    Ok(connect(family, &addr, deadline, &cancelled)?.map(TcpStream::from))
}

fn connect(
    family: AddressFamily,
    addr: &impl SocketAddrArg,
    deadline: Option<Instant>,
    cancelled: &dyn Fn() -> bool,
) -> Result<io::Result<OwnedFd>, MigrateError> {
    let fd = match nonblocking_socket(family) {
        Ok(fd) => fd,
        Err(e) => return Ok(Err(e)),
    };
    loop {
        if cancelled() {
            return Err(crate::cancelled());
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(Err(io::ErrorKind::TimedOut.into()));
        }
        match rustix::net::connect(&fd, addr) {
            Ok(()) | Err(Errno::ISCONN) => break,
            Err(Errno::INTR) => {}
            // Linux answers a full Unix backlog with EAGAIN and leaves
            // the socket unconnected, so the connect is simply retried.
            Err(Errno::AGAIN) => thread::sleep(CANCEL_POLL),
            Err(Errno::INPROGRESS | Errno::ALREADY) => {
                if !writable(&fd, CANCEL_POLL) {
                    continue;
                }
                match rustix::net::sockopt::socket_error(&fd) {
                    Ok(Ok(())) => break,
                    Ok(Err(e)) | Err(e) => return Ok(Err(e.into())),
                }
            }
            Err(e) => return Ok(Err(e.into())),
        }
    }
    // The protocol reads and writes with blocking calls.
    Ok(rustix::io::ioctl_fionbio(&fd, false)
        .map(|()| fd)
        .map_err(Into::into))
}

fn nonblocking_socket(family: AddressFamily) -> io::Result<OwnedFd> {
    let fd = rustix::net::socket_with(
        family,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    rustix::io::ioctl_fionbio(&fd, true)?;
    Ok(fd)
}

/// Whether a connect in progress finished inside `slice`.
fn writable(fd: &OwnedFd, slice: Duration) -> bool {
    let ts = Timespec {
        tv_sec: slice.as_secs() as _,
        tv_nsec: slice.subsec_nanos() as _,
    };
    let mut fds = [PollFd::new(fd, PollFlags::OUT)];
    // A failed poll is looked at again on the next turn.
    matches!(rustix::event::poll(&mut fds, Some(&ts)), Ok(n) if n > 0)
}

#[cfg(test)]
mod tests;
