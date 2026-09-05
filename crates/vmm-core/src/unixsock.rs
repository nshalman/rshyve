// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Restricted Unix domain socket creation and bounded writes.

use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static UMASK_LOCK: Mutex<()> = Mutex::new(());

/// Filesystem permissions for a Unix domain socket and its parent directory.
#[derive(Debug, Clone, Copy)]
pub struct SocketPolicy {
    pub mode: u32,
    pub dir_mode: u32,
}

impl Default for SocketPolicy {
    fn default() -> Self {
        Self {
            mode: 0o600,
            dir_mode: 0o700,
        }
    }
}

/// Bind a Unix domain socket and verify its ownership and permissions.
///
/// If the parent directory does not exist, it is created with `dir_mode`.
/// Any failure after the socket is bound removes the socket path before the
/// error is returned.
pub fn bind_restricted(
    path: &Path,
    policy: SocketPolicy,
) -> io::Result<UnixListener> {
    let _guard = UMASK_LOCK
        .lock()
        .map_err(|_| io::Error::other("socket umask lock poisoned"))?;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(policy.dir_mode);
        builder.create(parent)?;
    }

    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let old_umask =
        unsafe { libc::umask((!policy.mode & 0o777) as libc::mode_t) };
    let result = UnixListener::bind(path);
    unsafe {
        libc::umask(old_umask);
    }
    let listener = result?;

    if let Err(error) =
        fs::set_permissions(path, Permissions::from_mode(policy.mode))
    {
        return Err(remove_after_error(path, error));
    }

    verify_bound_path(path, policy.mode, unsafe { libc::geteuid() })?;
    Ok(listener)
}

/// Largest block handed to one `write` after `poll` reports the socket
/// writable.
///
/// `POLLOUT` promises that some space is free, not that all of `data`
/// fits. A measurement on illumos AF_UNIX puts the point where one
/// write starts to wait on the peer again between 128 KiB and 256 KiB,
/// so this stays far below it.
const WRITE_CHUNK: usize = 8 * 1024;

/// Write all of `data` to `stream`, or give up after `budget`.
///
/// Callers do this write with a lock held, or on a vCPU thread, so it
/// must have an end. Two usual ways to give it one do not work here:
///
/// - `SO_SNDTIMEO` does nothing on illumos. `setsockopt` reports
///   success and a `write` to a full AF_UNIX socket still blocks with
///   no end.
/// - `O_NONBLOCK` is a file-status flag, which `try_clone` shares. The
///   reader threads do blocking reads on a clone of the same socket,
///   so setting it here would break every read.
///
/// `poll` is what does work. `POLLOUT` stays clear while the socket is
/// full, and a write of at most `WRITE_CHUNK` after `POLLOUT` moves its
/// bytes and returns.
///
/// A budget that runs out returns `ErrorKind::TimedOut`. Part of `data`
/// may be on the socket by then, so the caller has to drop the
/// connection rather than write to it again.
pub fn write_all_bounded(
    stream: &UnixStream,
    data: &[u8],
    budget: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + budget;
    let fd = stream.as_raw_fd();
    let mut sink: &UnixStream = stream;
    let mut sent = 0;

    while sent < data.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the peer did not take the write within its budget",
            ));
        }
        if !poll_writable(fd, left)? {
            continue;
        }
        let end = data.len().min(sent + WRITE_CHUNK);
        match sink.write(&data[sent..end]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the peer took none of the write",
                ))
            }
            Ok(written) => sent += written,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // A write timeout on a platform where `SO_SNDTIMEO` does
            // work. The budget above decides whether to try again.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Wait for `fd` to take a write, or for `budget` to pass.
///
/// An error condition reports true as well, so that the write which
/// follows names the error rather than this guessing at it.
fn poll_writable(fd: RawFd, budget: Duration) -> io::Result<bool> {
    // At least one millisecond: a shorter budget would round to a poll
    // that returns at once, and the caller would spin on it.
    let timeout_ms = budget.as_millis().clamp(1, i32::MAX as u128) as i32;
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, timeout_ms) } < 0 {
        let error = io::Error::last_os_error();
        // A signal is not a failure. The caller re-reads its deadline.
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(pfd.revents
        & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)
        != 0)
}

fn verify_bound_path(
    path: &Path,
    expected_mode: u32,
    expected_uid: u32,
) -> io::Result<()> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return Err(remove_after_error(path, error)),
    };
    let actual_mode = metadata.permissions().mode() & 0o777;
    let actual_uid = metadata.uid();

    if actual_mode != expected_mode || actual_uid != expected_uid {
        let error = io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "bound socket verification failed: mode {actual_mode:#05o} uid {actual_uid}, expected mode {expected_mode:#05o} uid {expected_uid}",
            ),
        );
        return Err(remove_after_error(path, error));
    }

    Ok(())
}

fn remove_after_error(path: &Path, error: io::Error) -> io::Error {
    match fs::remove_file(path) {
        Ok(()) => error,
        Err(cleanup_error)
            if cleanup_error.kind() == io::ErrorKind::NotFound =>
        {
            error
        }
        Err(cleanup_error) => io::Error::new(
            error.kind(),
            format!("{error}; failed to remove socket: {cleanup_error}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Payload larger than any socket buffer, so the write cannot
    /// finish until the peer reads.
    const OVERSIZED: usize = 8 * 1024 * 1024;
    const BUDGET: Duration = Duration::from_millis(200);

    #[test]
    fn a_write_that_fits_is_not_delayed() {
        let (mut peer, device) = UnixStream::pair().expect("socketpair");
        write_all_bounded(&device, b"hello", BUDGET).expect("write");

        let mut got = [0u8; 5];
        peer.read_exact(&mut got).expect("read");
        assert_eq!(&got, b"hello");
    }

    #[test]
    fn an_empty_write_needs_no_budget() {
        let (_peer, device) = UnixStream::pair().expect("socketpair");
        write_all_bounded(&device, b"", Duration::ZERO).expect("write");
    }

    /// `SO_SNDTIMEO` reports success on illumos and bounds nothing, so
    /// this is the case that proves the poll is what supplies the end.
    #[test]
    fn a_peer_that_never_reads_does_not_hold_the_writer() {
        let (_peer, device) = UnixStream::pair().expect("socketpair");
        let started = Instant::now();
        let error = write_all_bounded(&device, &vec![0u8; OVERSIZED], BUDGET)
            .expect_err("a peer that reads nothing must not be waited on");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the write took {:?}",
            started.elapsed()
        );
    }

    /// A peer that reads, but too slowly. The write makes progress, so
    /// a bound that only watched the first blocked write would never
    /// fire.
    #[test]
    fn a_peer_that_reads_too_slowly_still_loses_the_write() {
        let (peer, device) = UnixStream::pair().expect("socketpair");
        let taken = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&taken);
        let slow = std::thread::spawn(move || {
            let mut peer = peer;
            let mut buf = [0u8; 512];
            loop {
                std::thread::sleep(BUDGET / 4);
                match peer.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => counted.fetch_add(n, Ordering::Release),
                };
            }
        });

        let started = Instant::now();
        let error = write_all_bounded(&device, &vec![0u8; OVERSIZED], BUDGET)
            .expect_err("a peer this slow must not be waited on");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the write took {:?}",
            started.elapsed()
        );

        // The socket buffer alone accepts far less than the payload,
        // so the peer had to take bytes for the write to get past it.
        assert!(
            taken.load(Ordering::Acquire) > 0,
            "the write made no progress"
        );
        // Wake the reader so this test leaks no thread. The peer may
        // already be gone, and there is nothing to do about that.
        let _ = device.shutdown(std::net::Shutdown::Both);
        slow.join().expect("join the slow reader");
    }

    #[test]
    fn socket_is_mode_0600_after_bind() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("control.sock");

        let _listener = bind_restricted(&path, SocketPolicy::default())
            .expect("bind restricted socket");
        let mode = fs::metadata(&path)
            .expect("stat socket")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600);
    }

    #[test]
    fn parent_dir_created_with_dir_mode() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let parent = temp.path().join("private");
        let path = parent.join("control.sock");

        let _listener = bind_restricted(&path, SocketPolicy::default())
            .expect("bind restricted socket");
        let mode = fs::metadata(&parent)
            .expect("stat parent directory")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o700);
    }

    #[test]
    fn bind_removes_stale_socket() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("control.sock");
        let stale = UnixListener::bind(&path).expect("bind stale socket");
        drop(stale);

        let listener = bind_restricted(&path, SocketPolicy::default())
            .expect("replace stale socket");
        let client =
            UnixStream::connect(&path).expect("connect to replacement socket");
        let _accepted =
            listener.accept().expect("accept replacement connection");
        drop(client);
    }

    #[test]
    fn bind_fails_when_mode_verification_fails() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("control.sock");
        let _listener = UnixListener::bind(&path).expect("bind test socket");
        fs::set_permissions(&path, Permissions::from_mode(0o666))
            .expect("set deliberately unsafe mode");

        let result =
            verify_bound_path(&path, 0o600, unsafe { libc::geteuid() });

        assert!(result.is_err());
        assert!(!path.exists());
    }
}
