// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Restricted Unix domain socket creation, bounded writes, and the
//! accept loop every listener in this workspace runs.

pub mod accept;
pub mod peercred;

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::net::Shutdown;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::{
    lchown, DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt,
};

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::poll::{wait_readable, wait_writable, Readiness};
use peercred::ListenerOwner;

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
/// A stale socket left by an earlier process is replaced; see
/// [`remove_stale_socket`] for what else may be at the path. Any failure
/// after the socket is bound removes the socket path before the error
/// is returned.
///
/// When the process peer policy names an owner, the socket is given to
/// it here, before the privilege drop that policy describes, so a peer
/// running as that identity can connect. See [`secure_chain`] for where.
/// The dropped process has no `file_write`, so it cannot unlink or
/// rebind it; the next start clears a stale socket.
pub fn bind_restricted(
    path: &Path,
    policy: SocketPolicy,
) -> io::Result<UnixListener> {
    bind_owned(path, policy, peercred::policy().owner, secure_chain)
}

/// [`bind_restricted`] with the owner given, so the chown can be tested
/// without installing a process-wide policy.
fn bind_owned(
    path: &Path,
    policy: SocketPolicy,
    owner: Option<ListenerOwner>,
    trusted: impl Fn(&Path) -> Option<PathBuf>,
) -> io::Result<UnixListener> {
    let _guard = UMASK_LOCK
        .lock()
        .map_err(|_| io::Error::other("socket umask lock poisoned"))?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // A directory made for an owner stays ours: the owner only needs to
    // search it to connect, and one it owned could hide a swap.
    let dir_mode = match owner {
        Some(_) => policy.dir_mode | 0o011,
        None => policy.dir_mode,
    };
    // For an owner only the last directory is made, with one mkdir, so
    // `created` is exact and no directory above it is left unsearchable.
    let created = match owner {
        Some(_) => match DirBuilder::new().mode(dir_mode).create(parent) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("{}: {error}", parent.display()),
                ))
            }
        },
        None => {
            DirBuilder::new()
                .recursive(true)
                .mode(dir_mode)
                .create(parent)?;
            false
        }
    };
    // Decided before the bind: on a secure path no other user can swap
    // a step between the bind and the chown. Every step after this one
    // uses the canonical path that was checked, so a link resolved here
    // is not resolved again.
    let secure = owner.and_then(|_| trusted(parent));
    let give_to = secure.as_ref().and(owner);
    let checked;
    let path = match &secure {
        Some(dir) => {
            let name = path.file_name().ok_or_else(|| {
                io::Error::other(format!("{} names no socket", path.display()))
            })?;
            checked = dir.join(name);
            checked.as_path()
        }
        None => path,
    };
    // The umask can take the search bits off a directory made here.
    // Set by path, so only on a secure one.
    if let (Some(dir), true) = (&secure, created) {
        fs::set_permissions(dir, Permissions::from_mode(dir_mode))?;
    }

    remove_stale_socket(path)?;

    // SAFETY: `umask` only reads its argument and returns the old mask.
    let old_umask = unsafe { libc::umask(socket_umask(policy.mode)) };
    let result = UnixListener::bind(path);
    // SAFETY: `umask` only reads its argument and returns the old mask.
    unsafe {
        libc::umask(old_umask);
    }
    let listener = result?;

    if let Err(error) =
        fs::set_permissions(path, Permissions::from_mode(policy.mode))
    {
        return Err(remove_after_error(path, error));
    }

    // SAFETY: geteuid takes no argument and cannot fail.
    let mut expected_uid = unsafe { libc::geteuid() };
    // Elsewhere the socket keeps the binding identity. The run-as
    // identity also needs search on every directory of the path.
    if let Some(owner) = give_to {
        match lchown(path, Some(owner.uid), Some(owner.gid)) {
            Ok(()) => expected_uid = owner.uid,
            // A zone's privilege floor has no file_chown. The socket then
            // keeps the binding identity too.
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(error) => return Err(remove_after_error(path, error)),
        }
    }

    verify_bound_path(path, policy.mode, expected_uid)?;
    Ok(listener)
}

/// The canonical form of `dir` when no user but root and this one can
/// change any step of it. Anything that cannot be read is not secure.
fn secure_chain(dir: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(dir).ok()?;
    // SAFETY: geteuid takes no argument and cannot fail.
    let euid = unsafe { libc::geteuid() };
    // From the root down, the ACL before the mode: a step is trusted
    // only once every step above it is.
    let steps: Vec<&Path> = canonical.ancestors().collect();
    for step in steps.into_iter().rev() {
        if !acl_trivial(step).ok()? {
            return None;
        }
        let meta = fs::symlink_metadata(step).ok()?;
        if !secure_step(meta.uid(), meta.mode(), euid) {
            return None;
        }
    }
    Some(canonical)
}

/// Whether `step` has no ACL beyond its mode bits. A ZFS ACL can let
/// another user add or remove entries that the mode does not show.
#[cfg(any(target_os = "illumos", target_os = "solaris"))]
fn acl_trivial(step: &Path) -> io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    #[link(name = "sec")]
    unsafe extern "C" {
        #[link_name = "acl_trivial"]
        fn libsec_acl_trivial(path: *const libc::c_char) -> libc::c_int;
    }
    let c_path = std::ffi::CString::new(step.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("a path with a NUL byte"))?;
    // SAFETY: `c_path` is a NUL-terminated string that outlives the call.
    match unsafe { libsec_acl_trivial(c_path.as_ptr()) } {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Only illumos ACLs are read; elsewhere the mode bits decide.
#[cfg(not(any(target_os = "illumos", target_os = "solaris")))]
fn acl_trivial(_step: &Path) -> io::Result<bool> {
    Ok(true)
}

/// Owned by root or by us, and writable by nobody else. A sticky
/// directory is not enough: another user can make the next step in it.
fn secure_step(uid: u32, mode: u32, euid: u32) -> bool {
    (uid == 0 || uid == euid) && mode & 0o022 == 0
}

/// The umask that keeps the socket within `mode` between its `bind`
/// and the `chmod` that follows.
///
/// Only the read and write bits are masked. A connect to a bound
/// AF_UNIX socket is checked as read plus write (illumos
/// `so_ux_addr_xlate`, `VOP_ACCESS(vp, VREAD|VWRITE)`), so an execute
/// bit left on the socket grants nobody anything. Masking one costs a
/// lot: the umask is process-wide, so it also takes the search bit off
/// every directory another thread creates in the window, and every
/// later open below that directory fails with `EACCES`.
fn socket_umask(mode: u32) -> libc::mode_t {
    (!mode & 0o666) as libc::mode_t
}

/// Unlink `path` only when it is a socket nobody listens on.
///
/// The path comes from the operator: a `-s` spec, or a `device-add`
/// over the control channel for the life of the VM. An unconditional
/// unlink would remove whatever file that names. So the inode has to
/// be a socket, checked without following a symlink, and a connect to
/// it has to be refused, which is what a socket whose listener has
/// gone answers with. A connect that succeeds means another process
/// serves the path, and that process keeps it. A listener whose backlog
/// is full also refuses the connect (illumos `tl_conn_req`), so a busy
/// socket can still be taken; nothing short of asking the kernel who
/// holds the bind distinguishes the two, and illumos offers no such
/// call.
///
/// The check and the unlink are two steps, so a parent directory the
/// caller does not control can still swap the inode between them.
/// The socket directory is created 0700 above for that reason.
fn remove_stale_socket(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a socket", path.display()),
        ));
    }
    match UnixStream::connect(path) {
        Ok(_live) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{} is served by another process", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            fs::remove_file(path)
        }
        Err(error) => Err(error),
    }
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
/// # One writer per socket
///
/// The `poll` and the send are two steps. A second writer that takes
/// the reported space between them leaves this send with nowhere to
/// put its bytes, and illumos then waits for the peer inside the send,
/// with no timeout (`strwaitq`, see [`WRITE_CHUNK`]). The budget does
/// not cover that wait, so a socket with two writers is not bounded by
/// this function.
///
/// This function cannot enforce the rule. A caller holds it by giving
/// each socket one writer: a thread that owns the socket, or a lock
/// that every write path takes. A reader thread on a `try_clone` of
/// the same socket is not a second writer.
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
    let fd = stream.as_fd();
    let mut sent = 0;

    while sent < data.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the peer did not take the write within its budget",
            ));
        }
        if !wait_writable(fd, left)? {
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
/// An AF_UNIX send reaches `strwrite_common` (os/streamio.c). That
/// cuts the write into messages of `sd_qn_maxpsz` and, between two of
/// them, can sleep in `strwaitq` with no timeout, and with signals
/// masked after the first message. `tl` asks for `INFPSZ`, but the
/// stream head caps it at `strmsgsz`, 64 KiB by default
/// (`conf/param.c`). So a write above that cap can wait for the peer.
/// A write below it is one message.
///
/// One message does not wait. `strput` calls `putnext` once
/// `canputnext` holds, and `putnext` places the whole message whatever
/// its size: STREAMS flow control is advisory, so the queue may pass
/// its high-water mark. `POLLOUT` reads the same `QFULL` bit
/// `canputnext` does (`strpoll`, os/streamio.c). The send after a
/// `POLLOUT` therefore places its message and returns. Measured on
/// illumos: an 8 KiB send after `POLLOUT` returns at once, and
/// `POLLOUT` stays clear while the queue is full.
///
/// 8 KiB is one message for any `strmsgsz` of 8 KiB or more. A host
/// tuned below that loses the bound.
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
fn send_nowait(fd: BorrowedFd<'_>, data: &[u8]) -> io::Result<usize> {
    // SAFETY: `data` is a live slice for the length passed.
    let sent = unsafe {
        libc::send(
            fd.as_raw_fd(),
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

/// Close a connection after its last write without losing that write.
///
/// A close on illumos can drop bytes the peer has not read yet, though
/// every write returned `Ok`, and `SO_LINGER` does not stop it. In
/// `tl` (io/tl.c):
///
/// - A close after `shutdown(Write)` sends the peer a disconnect, and
///   `tl_discon_ind` flushes the peer's read queue first.
/// - A close while data waits on this end's queue for a slow peer can
///   race `tl_data`: it reads `te_closing` once, and its `TL_PUTBQ`
///   then stops the drain in `tl_close_ser`.
///
/// So this end shuts down its writes and waits, up to `budget`, for
/// the peer to close, which it does once it has read to the end. This
/// needs a peer that does so: one that stops reading still loses what
/// it left when the budget runs out. Bytes that the peer sends in the
/// meantime are dropped.
///
/// A peer that already shut down its own writes gets a plain close, so
/// the race above can still reach it. A shutdown would be worse: both
/// ends are then idle, the close sends the flushing disconnect, and
/// `poll` reports a hangup at once, so no wait can see the peer finish.
/// The same holds for a peer that shuts down its writes after the
/// check here but before it has read to the end.
pub fn close_after_write(stream: UnixStream, budget: Duration) {
    let deadline = Instant::now() + budget;
    let fd = stream.as_fd();
    if drain_input(fd, deadline) != Drained::Empty {
        return;
    }
    if stream.shutdown(Shutdown::Write).is_err() {
        return;
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        match wait_readable(fd, left) {
            Readiness::Idle => continue,
            Readiness::Gone => return,
            Readiness::Readable => {}
        }
        if drain_input(fd, deadline) != Drained::Empty {
            return;
        }
    }
}

/// What [`drain_input`] found.
#[derive(Debug, PartialEq, Eq)]
enum Drained {
    /// Nothing more to read for now.
    Empty,
    /// The peer shut down its writes or closed.
    Closed,
    /// The read failed, or the deadline passed.
    Stop,
}

/// Read and drop what the peer has sent, without waiting for more.
fn drain_input(fd: BorrowedFd<'_>, deadline: Instant) -> Drained {
    let mut scratch = [0u8; 4096];
    loop {
        if Instant::now() >= deadline {
            return Drained::Stop;
        }
        // SAFETY: `scratch` is a live buffer for the length passed.
        let got = unsafe {
            libc::recv(
                fd.as_raw_fd(),
                scratch.as_mut_ptr().cast::<libc::c_void>(),
                scratch.len(),
                libc::MSG_DONTWAIT,
            )
        };
        match got {
            0 => return Drained::Closed,
            n if n > 0 => {}
            _ => match io::Error::last_os_error().kind() {
                io::ErrorKind::WouldBlock => return Drained::Empty,
                io::ErrorKind::Interrupted => {}
                _ => return Drained::Stop,
            },
        }
    }
}

fn verify_bound_path(
    path: &Path,
    expected_mode: u32,
    expected_uid: u32,
) -> io::Result<()> {
    // Not followed: a link swapped in after the bind is not our socket.
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => metadata,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is no longer the bound socket", path.display()),
            ))
        }
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
    use std::sync::mpsc;

    /// Payload larger than any socket buffer, so the write cannot
    /// finish until the peer reads.
    const OVERSIZED: usize = 8 * 1024 * 1024;
    const BUDGET: Duration = Duration::from_millis(200);

    /// How long the harness waits for a bounded write to return.
    ///
    /// A write that is not bounded never returns at all, so this only
    /// has to be clear of the budget. It turns that hang into one
    /// failed test rather than a test run that never ends.
    const LIMIT: Duration = Duration::from_secs(2);

    /// Put the socket in the state a bounded write has to survive:
    /// space free, but less than [`WRITE_CHUNK`] of it.
    ///
    /// This is what makes the test below exercise the send. A payload
    /// that starts on an empty socket fills it in whole blocks, so a
    /// send that waits for the peer is never reached, and the test
    /// passes whether the send is bounded or not.
    fn part_fill(device: &UnixStream) {
        // An odd length, so the space left is not a round number.
        write_all_bounded(device, &[0u8; 24], BUDGET).expect("prefix");
    }

    /// Run one bounded write on a worker, and give up on it after
    /// [`LIMIT`].
    ///
    /// The write is the thing under test, so it cannot be joined here:
    /// an unbounded one would park the test run instead of failing.
    fn write_within_limit(device: UnixStream, data: Vec<u8>) -> io::Result<()> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(write_all_bounded(&device, &data, BUDGET));
        });
        rx.recv_timeout(LIMIT)
            .expect("the write must end on its budget, not on the peer")
    }

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
        part_fill(&device);

        let error = write_within_limit(device, vec![0u8; OVERSIZED])
            .expect_err("a peer that reads nothing must not be waited on");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    /// A peer that reads once and then stops.
    ///
    /// The write makes progress, so a bound that only watched a write
    /// which never moved would never fire. The single read also leaves
    /// the socket with room for less than one [`WRITE_CHUNK`], which is
    /// the state a send that waits for the peer cannot get out of.
    #[test]
    fn a_peer_that_stops_reading_still_loses_the_write() {
        let (peer, device) = UnixStream::pair().expect("socketpair");
        // The reader reports what it took over a channel, not through
        // shared memory. A plain load races the reader thread: the
        // write can give up on its budget before the store lands, and
        // the check below then reads a count the peer has already
        // taken. `recv` waits for the reader instead.
        let (took, taken) = mpsc::channel::<usize>();
        // Dropped at the end of the test. Until then the peer stays
        // open: one that closed would end the write on a broken pipe,
        // which is not the case under test.
        let (hold, parked) = mpsc::channel::<()>();
        let reader = std::thread::spawn(move || {
            let mut peer = peer;
            let mut buf = [0u8; 4096];
            // Long enough for the write to fill the socket first, and
            // short enough to leave the write some budget after it.
            std::thread::sleep(BUDGET / 4);
            let _ = took.send(peer.read(&mut buf).unwrap_or(0));
            let _ = parked.recv();
        });

        let error = write_within_limit(device, vec![0u8; OVERSIZED])
            .expect_err("a peer that stopped reading must not be waited on");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let read = taken
            .recv_timeout(LIMIT)
            .expect("the peer must report what it read");
        assert!(read > 0, "the peer read nothing");

        drop(hold);
        reader.join().expect("join the reader");
    }

    /// The writes that lost the last send on illumos: a short one, a
    /// long one in chunks, then the close.
    fn write_then_close(device: UnixStream, big: &[u8]) {
        write_all_bounded(&device, &[1u8; 52], LIMIT).expect("short write");
        write_all_bounded(&device, big, LIMIT).expect("long write");
        close_after_write(device, LIMIT);
    }

    /// A plain close lost the tail in about one round in 3,000, and a
    /// shutdown then a close in about one in 100.
    #[test]
    fn a_close_after_write_delivers_the_tail() {
        let big = vec![7u8; 1_920_000];
        for round in 0..300 {
            let (mut peer, device) = UnixStream::pair().expect("socketpair");
            let reader = std::thread::spawn(move || {
                let mut got = Vec::new();
                peer.read_to_end(&mut got).map(|_| got.len())
            });
            write_then_close(device, &big);
            let got = reader.join().expect("join").expect("read");
            assert_eq!(got, 52 + big.len(), "round {round}");
        }
    }

    #[test]
    fn a_peer_that_never_closes_does_not_hold_the_close() {
        let (_peer, device) = UnixStream::pair().expect("socketpair");
        let start = Instant::now();
        close_after_write(device, BUDGET);
        let took = start.elapsed();
        assert!(took >= BUDGET, "returned before the budget: {took:?}");
        assert!(took < LIMIT, "held past the limit: {took:?}");
    }

    /// A directory another thread creates while the bind holds the
    /// umask must keep its search bit.
    #[test]
    fn the_socket_umask_masks_no_execute_bit() {
        let mask = socket_umask(0o600);
        assert_eq!(mask, 0o066, "an execute bit is masked");
        assert_eq!(0o700 & !mask, 0o700, "a 0700 directory lost a bit");
        assert_eq!(
            0o777 & !mask & 0o066,
            0,
            "group or other can read or write the socket"
        );
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

    /// The identity to give a socket to in a test: a real second id
    /// when the test runs as root, and otherwise the test's own, which
    /// still runs the chown and the verify against the owner.
    fn test_owner() -> ListenerOwner {
        // SAFETY: geteuid and getegid take no argument and cannot fail.
        let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
        if euid == 0 {
            ListenerOwner {
                uid: 8123,
                gid: 8123,
            }
        } else {
            ListenerOwner {
                uid: euid,
                gid: egid,
            }
        }
    }

    fn trust_all(dir: &Path) -> Option<PathBuf> {
        fs::canonicalize(dir).ok()
    }

    fn trust_none(_: &Path) -> Option<PathBuf> {
        None
    }

    /// The socket belongs to the identity the process will drop to. Its
    /// directory stays ours, searchable so that identity can connect.
    #[test]
    fn an_owner_takes_the_socket_and_can_reach_it() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let parent = temp.path().join("private");
        let path = parent.join("control.sock");
        let owner = test_owner();

        let _listener =
            bind_owned(&path, SocketPolicy::default(), Some(owner), trust_all)
                .expect("bind with an owner");

        let socket = fs::metadata(&path).expect("stat socket");
        assert_eq!(socket.uid(), owner.uid);
        assert_eq!(socket.gid(), owner.gid);
        assert_eq!(socket.permissions().mode() & 0o777, 0o600);
        let dir = fs::metadata(&parent).expect("stat parent directory");
        // SAFETY: geteuid takes no argument and cannot fail.
        assert_eq!(dir.uid(), unsafe { libc::geteuid() });
        assert_eq!(dir.permissions().mode() & 0o011, 0o011);
    }

    /// Where the path is not safe from other users, the socket keeps
    /// the binding identity.
    #[test]
    fn an_untrusted_path_keeps_the_binding_identity() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("control.sock");

        let _listener = bind_owned(
            &path,
            SocketPolicy::default(),
            Some(test_owner()),
            trust_none,
        )
        .expect("bind with an owner");

        let socket = fs::metadata(&path).expect("stat socket");
        // SAFETY: geteuid takes no argument and cannot fail.
        assert_eq!(socket.uid(), unsafe { libc::geteuid() });
    }

    #[test]
    fn only_steps_no_one_else_can_write_are_secure() {
        assert!(secure_step(0, 0o40755, 1000), "root's");
        assert!(secure_step(1000, 0o40700, 1000), "ours");
        assert!(!secure_step(0, 0o41777, 0), "a sticky /tmp");
        assert!(!secure_step(1000, 0o40755, 0), "another owner");
        assert!(!secure_step(0, 0o40775, 0), "group writable");
    }

    #[test]
    fn an_owner_needs_the_directories_above_to_exist() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("a").join("b").join("control.sock");
        bind_owned(
            &path,
            SocketPolicy::default(),
            Some(test_owner()),
            trust_all,
        )
        .expect_err("a missing directory above");
        assert!(!temp.path().join("a").exists());
    }

    #[test]
    fn the_root_directory_has_a_trivial_acl() {
        assert!(acl_trivial(Path::new("/")).expect("read the ACL"));
    }

    /// A socket reached through a link is bound and given away at the
    /// canonical path that was checked.
    #[test]
    fn a_trusted_path_is_used_in_its_canonical_form() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let target = temp.path().join("target");
        fs::create_dir(&target).expect("create the target");
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        let owner = test_owner();

        let _listener = bind_owned(
            &link.join("control.sock"),
            SocketPolicy::default(),
            Some(owner),
            trust_all,
        )
        .expect("bind through the link");

        let socket = fs::symlink_metadata(target.join("control.sock"))
            .expect("bound in the target");
        assert!(socket.file_type().is_socket());
        assert_eq!(socket.uid(), owner.uid);
    }

    /// Without an owner the bind leaves the process's own ids, so a
    /// binary that never drops changes nothing it did not create.
    #[test]
    fn no_owner_keeps_the_binding_identity() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("control.sock");

        let _listener =
            bind_owned(&path, SocketPolicy::default(), None, trust_all)
                .expect("bind without an owner");

        let socket = fs::metadata(&path).expect("stat socket");
        // SAFETY: geteuid takes no argument and cannot fail.
        assert_eq!(socket.uid(), unsafe { libc::geteuid() });
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

    /// `-s 5,virtio-console,/etc/passwd` must fail without removing
    /// the file it names.
    #[test]
    fn bind_does_not_remove_a_file_that_is_not_a_socket() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("control.sock");
        fs::write(&path, b"keep me").expect("write a regular file");

        let error = bind_restricted(&path, SocketPolicy::default())
            .expect_err("a regular file must not be replaced");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&path).expect("the file is still there"),
            b"keep me"
        );
    }

    /// A symlink is not followed: the target is not stat'ed and the
    /// link is not removed.
    #[test]
    fn bind_does_not_remove_a_symlink_to_a_socket() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let target = temp.path().join("real.sock");
        let path = temp.path().join("control.sock");
        let _stale = UnixListener::bind(&target).expect("bind the target");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");

        let error = bind_restricted(&path, SocketPolicy::default())
            .expect_err("a symlink must not be replaced");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(fs::symlink_metadata(&path)
            .expect("the link is still there")
            .file_type()
            .is_symlink());
    }

    /// A second VM that names a running VM's socket must not take it.
    #[test]
    fn bind_does_not_remove_a_socket_another_listener_serves() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("control.sock");
        let live = UnixListener::bind(&path).expect("bind the live socket");

        let error = bind_restricted(&path, SocketPolicy::default())
            .expect_err("a served socket must not be replaced");

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        // The probe connect is the one the live listener sees; after
        // it the path still reaches this listener.
        let _probe = live.accept().expect("the probe connection");
        let client = UnixStream::connect(&path).expect("still served");
        let _accepted = live.accept().expect("accept after the refusal");
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
            // SAFETY: geteuid takes no argument and cannot fail.
            verify_bound_path(&path, 0o600, unsafe { libc::geteuid() });

        assert!(result.is_err());
        assert!(!path.exists());
    }
}
