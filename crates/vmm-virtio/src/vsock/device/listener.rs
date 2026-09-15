// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The host socket: the accept loop, the opening verb, and the reader
//! thread each connection gets.
//!
//! # Invariant
//!
//! Every thread here ends on the shutdown flag alone. The listener
//! polls instead of blocking in `accept`, a peer that sends no verb is
//! dropped on the handshake budget, and a reader's blocked `read` wakes
//! on its own poll. So halt does not connect to a socket to wake a
//! thread, and never waits on a host peer to tear the device down.

use std::io::{self, Read};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vmm_core::unixsock::write_all_bounded;

use super::shared::{VsockShared, HOST_WRITE_TIMEOUT};
use crate::vsock::control::{
    read_line_bounded, run_control_session, LineRead, HANDSHAKE_BUDGET,
};
use crate::vsock::packet::ConnKey;

/// Bytes read from a host socket in one go.
const HOST_READ_CHUNK: usize = 64 * 1024;
/// Pause before retrying a host read when the guest is not draining.
const BACKPRESSURE_PAUSE: Duration = Duration::from_millis(2);
/// How long a blocked host read waits before it looks at the shutdown
/// flag. A reader that only wakes on the socket being shut down cannot
/// end on its own, and halt then pays the whole budget for it.
const HOST_READ_POLL: Duration = Duration::from_millis(250);
/// Longest CONNECT line accepted, so a client that never sends a newline
/// cannot make the device read without bound.
pub(super) const CONNECT_LINE_MAX: usize = 64;

/// Accept host-initiated connections and parse their CONNECT line.
pub(super) fn accept_loop(shared: Arc<VsockShared>, listener: UnixListener) {
    let on_client = |stream: UnixStream| {
        // Every accepted connection costs a thread until its peer sends
        // a CONNECT line, which it need never do.
        if !shared.reader_slot_free() {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        let conn_shared = Arc::clone(&shared);
        // One thread per pending connection: the CONNECT line is read
        // with a blocking read, and a client that never sends one must
        // not block every other client behind it.
        if let Ok(handle) = std::thread::Builder::new()
            .name("vsock-connect".into())
            .spawn(move || handle_host_connect(conn_shared, stream))
        {
            shared.track_reader(handle);
        }
    };
    crate::socket_accept::accept_loop(
        listener,
        &shared.shutdown,
        &shared.log,
        "virtio-vsock",
        on_client,
    );
}

/// The verb a host peer sent on its first line.
pub(super) enum HostVerb {
    /// Firecracker's `CONNECT <port>`: stream to a guest port.
    Connect(u32),
    /// `CONTROL`: the VMM answers this connection itself and the guest
    /// mux never sees it.
    Control,
    Unknown,
}

pub(super) fn classify_host_line(line: &str) -> HostVerb {
    if let Some(port) = parse_connect_line(line) {
        return HostVerb::Connect(port);
    }
    if line.trim_end_matches(['\r', '\n']).trim() == "CONTROL" {
        return HostVerb::Control;
    }
    HostVerb::Unknown
}

/// Read the opening line and act on the verb it names.
pub(super) fn handle_host_connect(
    shared: Arc<VsockShared>,
    mut stream: UnixStream,
) {
    // The reader arms its own poll and checks the shutdown flag on
    // every pass. A peer that opens a connection and sends nothing
    // would otherwise park this thread for the life of the VM, and it
    // is not in `sockets` yet, so halt's sweep cannot wake it either.
    let LineRead::Line(line) = read_line_bounded(
        &mut stream,
        &shared.shutdown,
        CONNECT_LINE_MAX,
        Some(Instant::now() + HANDSHAKE_BUDGET),
    ) else {
        return;
    };
    match classify_host_line(&line) {
        HostVerb::Connect(port) => start_host_connection(shared, stream, port),
        HostVerb::Control => run_control_session(
            &shared.control,
            &shared.shutdown,
            stream,
            &shared.log,
        ),
        HostVerb::Unknown => {
            // Best effort: the peer may already be gone, and the
            // connection ends either way. Bounded, because a peer that
            // never reads would otherwise hold this thread, and one of
            // the reader slots, for the life of the VM.
            let _ = write_all_bounded(
                &stream,
                b"ERROR: expected CONNECT <port>\n",
                HOST_WRITE_TIMEOUT,
            );
        }
    }
}

/// Join a host peer to a guest port and stream until it closes.
pub(super) fn start_host_connection(
    shared: Arc<VsockShared>,
    stream: UnixStream,
    port: u32,
) {
    let key = ConnKey {
        guest_port: port,
        host_port: shared.mux.alloc_host_port(),
    };
    let s = stream;
    let Ok(peer) = s.try_clone() else {
        return;
    };
    // The table is capped, and a peer told OK for a connection the mux
    // refused would wait on a guest that is never going to hear about
    // it. Reserving the slot answers that without letting the guest
    // reach the connection yet.
    if !shared.mux.reserve_host_connect(key) {
        // Best effort: the peer may already be gone.
        let _ = write_all_bounded(
            &s,
            b"ERROR: too many connections\n",
            HOST_WRITE_TIMEOUT,
        );
        return;
    }
    // This thread owns the socket until the REQUEST is queued. Once it
    // is, any thread's deliver_rx may hand it to the guest, the guest
    // may answer and send payload, and a vCPU may then be inside
    // write_host on this same socket. Two writers would interleave the
    // OK line with guest payload and break the handshake framing, and
    // each would take the space the other's poll reported. So the OK
    // line goes out first, and the socket goes in the table before the
    // REQUEST, because payload for a key with no socket has nowhere to
    // go.
    if write_all_bounded(
        &s,
        format!("OK {}\n", key.host_port).as_bytes(),
        HOST_WRITE_TIMEOUT,
    )
    .is_err()
    {
        shared.mux.forget(key);
        return;
    }
    shared.register_socket(key, peer);
    shared.mux.announce_host_connect(key);
    shared.deliver_rx();
    reader_loop(shared, key, s);
}

/// Read from a connection's host socket until it closes.
pub(super) fn reader_loop(
    shared: Arc<VsockShared>,
    key: ConnKey,
    mut stream: UnixStream,
) {
    // Arm the poll here rather than inherit whatever the CONNECT
    // handshake left on the socket: a timeout the loop did not ask for
    // would read as a dead peer, and no timeout at all leaves the
    // shutdown check below unreachable.
    let polled = stream.set_read_timeout(Some(HOST_READ_POLL)).is_ok();
    let mut buf = vec![0u8; HOST_READ_CHUNK];
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // An idle poll, not a dead peer. Round the loop so the
            // shutdown flag above is read again.
            Err(e)
                if polled
                    && matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
            {
                continue
            }
            Err(_) => break,
        };

        // The guest's receive window and the backlog each bound what
        // may go at once, so one read can take several passes. The mux
        // removes what it took, so `data` always holds exactly what the
        // guest has not been given yet: no byte is offered twice, and
        // none is dropped.
        let mut data = buf[..n].to_vec();
        loop {
            shared.mux.queue_host_data(key, &mut data);
            shared.deliver_rx();
            if data.is_empty() {
                break;
            }
            if shared.shutdown.load(Ordering::Acquire) {
                return;
            }
            // The window opens again on a packet from the guest, and
            // backlog room comes back as the guest posts RX chains.
            // Both land on other threads, so this waits for them rather
            // than spinning. A guest that never reads stalls this
            // connection alone: one reader thread and one host socket,
            // both already capped.
            std::thread::sleep(BACKPRESSURE_PAUSE);
        }
    }

    shared.mux.queue_host_close(key);
    shared.drop_socket(key);
    shared.deliver_rx();
}

/// Parse Firecracker's `CONNECT <port>` line.
pub fn parse_connect_line(line: &str) -> Option<u32> {
    let rest = line
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("CONNECT ")?;
    rest.trim().parse().ok()
}
