// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Restricted Unix domain socket creation and bounded writes.

use std::fs::{self, DirBuilder, Permissions};
use std::io;
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
/// `poll` supplies the wait, and [`WRITE_CHUNK`] plus the per-call
/// flag in [`SEND_NOWAIT`] keep the send that follows it short enough
/// not to wait again.
///
/// This narrows the window rather than closing it. Between the `poll`
/// and the send, another writer on the same socket can take the space
/// the `poll` reported, and illumos then waits for the peer inside the
/// send. Every caller here has one writer per socket, so nothing takes
/// that space, but the socket type does not enforce it.
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
        match send_nowait(fd, &data[sent..end]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the peer took none of the write",
                ))
            }
            Ok(written) => sent += written,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // The socket filled between the poll and the send, or it
            // had less space than `POLLOUT` implied. The budget above
            // decides whether to try again. `TimedOut` covers a
            // platform where `SO_SNDTIMEO` does work.
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

/// Largest block handed to one send after `poll` reports space.
///
/// illumos has no flag that makes an AF_UNIX send give up.
/// `MSG_DONTWAIT` reaches `FNONBLOCK` on the read path only
/// (`sotpi_sendmsg`, socktpi.c), and `SO_SNDTIMEO` bounds nothing. The
/// size of the send is the only control left.
///
/// An AF_UNIX send reaches `strwrite_common`, which cuts the write
/// into messages of `sd_qn_maxpsz` and can wait for the peer between
/// two of them, untimed. `tl` asks for `INFPSZ`, but the stream head
/// caps that at `strmsgsz`, 64 KiB by default (`conf/param.c`). One
/// message is placed whole once `canputnext` holds, which is what
/// `POLLOUT` reports, so a send below that cap does not reach the
/// wait. This stays far enough below to survive a lowered `strmsgsz`.
const WRITE_CHUNK: usize = 8 * 1024;

/// Flags that stop one send from waiting for the peer.
///
/// Darwin counts free bytes rather than messages, and reports
/// `POLLOUT` on as little as `SO_SNDLOWAT` of them, so even a
/// [`WRITE_CHUNK`] send waits for the peer after `POLLOUT`. `sosend`
/// reads `MSG_NBIO` for this, not `MSG_DONTWAIT`. The tests run on
/// Darwin, so the bound has to hold there as well.
#[cfg(target_vendor = "apple")]
const SEND_NOWAIT: libc::c_int = 0x0002_0000; // MSG_NBIO
#[cfg(not(target_vendor = "apple"))]
const SEND_NOWAIT: libc::c_int = 0;

/// Send what the socket will take now.
fn send_nowait(fd: RawFd, data: &[u8]) -> io::Result<usize> {
    let sent = unsafe {
        libc::send(
            fd,
            data.as_ptr().cast::<libc::c_void>(),
            data.len(),
            SEND_NOWAIT,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sent as usize)
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

    /// The socket part full when the write starts.
    ///
    /// `POLLOUT` reports space that is smaller than the rest of the
    /// payload, so a send that could wait for the peer waits here and
    /// nowhere else. A payload that starts on an empty socket can fill
    /// it in whole blocks and miss this.
    #[test]
    fn a_write_onto_a_part_full_socket_is_still_bounded() {
        let (_peer, device) = UnixStream::pair().expect("socketpair");
        // A short odd prefix, so the space left is not a round number.
        write_all_bounded(&device, &[0u8; 24], BUDGET).expect("prefix");

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
