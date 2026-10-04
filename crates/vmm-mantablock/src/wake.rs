// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The wake-up pipes.
//!
//! The ends arrive blocking. Both are made non-blocking here, so a full
//! pipe never stalls a vCPU in `submit`.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{FileType, OFlags};
use rustix::io::Errno;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    Read,
    Write,
}

pub(crate) fn adopt(fd: &OwnedFd, end: End) -> io::Result<()> {
    let st = rustix::fs::fstat(fd)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::Fifo {
        return Err(bad("a wake-up descriptor is not a pipe"));
    }
    let fl = rustix::fs::fcntl_getfl(fd)?;
    // illumos pipes are STREAMS pipes: both ends are open for reading and
    // writing, so only the wrong single direction is refused.
    let wrong = match end {
        End::Read => OFlags::WRONLY,
        End::Write => OFlags::RDONLY,
    };
    if fl & OFlags::RWMODE == wrong {
        return Err(bad("a wake-up pipe end has the wrong direction"));
    }
    rustix::fs::fcntl_setfl(fd, fl | OFlags::NONBLOCK)?;
    Ok(())
}

/// A full pipe already holds a wake-up, which is enough.
pub(crate) fn wake(fd: &OwnedFd) -> io::Result<()> {
    loop {
        match rustix::io::write(fd, &[1u8]) {
            Ok(_) | Err(Errno::AGAIN) => return Ok(()),
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Waited {
    /// A wake-up came, or the time ran out. Look at the ring either way.
    Look,
    /// The engine closed its end, or the pipe failed.
    Gone,
}

/// One read of at most 64 bytes, so an engine that fills the pipe
/// cannot keep this thread reading.
pub(crate) fn wait(fd: &OwnedFd, limit: Duration) -> Waited {
    let ts = Timespec {
        tv_sec: limit.as_secs() as _,
        tv_nsec: limit.subsec_nanos() as _,
    };
    let mut fds = [PollFd::from_borrowed_fd(fd.as_fd(), PollFlags::IN)];
    match rustix::event::poll(&mut fds, Some(&ts)) {
        Ok(0) | Err(Errno::INTR) => return Waited::Look,
        Ok(_) => {}
        Err(_) => return Waited::Gone,
    }
    let rev = fds[0].revents();
    if rev.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL) {
        return Waited::Gone;
    }
    let mut b = [0u8; 64];
    match rustix::io::read(fd, &mut b) {
        Ok(0) => Waited::Gone,
        Ok(_) | Err(Errno::AGAIN) | Err(Errno::INTR) => Waited::Look,
        Err(_) => Waited::Gone,
    }
}

fn bad(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ends_are_checked_and_made_non_blocking() {
        let (rx, tx) = rustix::pipe::pipe().unwrap();
        // Elsewhere each end goes one way; illumos pipes go both.
        #[cfg(not(target_os = "illumos"))]
        {
            assert!(adopt(&rx, End::Write).is_err());
            assert!(adopt(&tx, End::Read).is_err());
        }
        adopt(&rx, End::Read).unwrap();
        adopt(&tx, End::Write).unwrap();
        // A full pipe does not block the waker.
        for _ in 0..100_000 {
            wake(&tx).unwrap();
        }
        assert_eq!(wait(&rx, Duration::ZERO), Waited::Look);
        drop(tx);
        // Drain what is left, then see the closed end.
        let mut gone = false;
        for _ in 0..100_000 {
            if wait(&rx, Duration::ZERO) == Waited::Gone {
                gone = true;
                break;
            }
        }
        assert!(gone);

        let file = tempfile::tempfile().unwrap();
        assert!(adopt(&OwnedFd::from(file), End::Read).is_err());
    }

    #[test]
    fn a_wake_to_a_closed_engine_end_fails() {
        let (rx, tx) = rustix::pipe::pipe().unwrap();
        adopt(&tx, End::Write).unwrap();
        drop(rx);
        assert!(wake(&tx).is_err());
    }
}
