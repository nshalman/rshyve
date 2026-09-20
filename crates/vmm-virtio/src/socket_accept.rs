// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The accept loop shared by the devices that serve a unix socket.
//!
//! The loop needs a wait it can leave. A blocking `accept` ends only
//! when a client arrives, so halt would have to connect to the socket
//! itself to wake it, and halt must not depend on a peer. That connect
//! is also not dependable: illumos `tl_conn_req` refuses it with
//! ECONNREFUSED once the listener's backlog is full
//! (uts/common/io/tl.c), which leaves the accept thread asleep. So the
//! listener is non-blocking and the loop polls it on a tick,
//! re-reading the shutdown flag each time.

use std::io;
use std::net::Shutdown;
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use slog::{debug, warn, Logger};
use vmm_core::poll::{wait_readable, Readiness};

/// How long the accept thread waits before it re-reads the shutdown
/// flag. This bounds how long halt waits for that thread to return.
const ACCEPT_POLL: Duration = Duration::from_millis(50);
/// Pause after a failed `accept`, so a persistent error such as EMFILE
/// cannot spin the accept thread at full speed.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Hand back an accepted socket the readers can use, or nothing.
///
/// The listener is non-blocking so the accept loop can poll. On BSD and
/// illumos an accepted socket inherits that flag. Every reader on this
/// socket arms a read timeout and treats `WouldBlock` as an idle poll,
/// so a socket that kept the flag would spin a thread at full speed
/// instead of waiting. A socket the flag cannot be cleared on is closed
/// rather than served.
pub(crate) fn accepted_blocking(stream: UnixStream) -> Option<UnixStream> {
    match stream.set_nonblocking(false) {
        Ok(()) => Some(stream),
        Err(_) => {
            let _ = stream.shutdown(Shutdown::Both);
            None
        }
    }
}

/// Accept clients until `shutdown`, handing each usable socket to
/// `on_client`.
///
/// A persistent accept failure is reported once per run of failures.
/// It is seen again on every pass, and a peer that can make one happen
/// must not be able to drive the log with it.
pub(crate) fn accept_loop(
    listener: UnixListener,
    shutdown: &AtomicBool,
    log: &Logger,
    device: &'static str,
    mut on_client: impl FnMut(UnixStream),
) {
    if let Err(error) = listener.set_nonblocking(true) {
        // Without the flag this thread would block in `accept` and only
        // a client could end it. Serving no connection is the lesser
        // fault: a device that cannot be torn down holds the whole
        // teardown sweep.
        warn!(log, "listener setup failed";
            "device" => device, "error" => %error);
        return;
    }

    let mut reported = false;
    while !shutdown.load(Ordering::Acquire) {
        match wait_readable(listener.as_fd(), ACCEPT_POLL) {
            Readiness::Readable => {}
            Readiness::Idle => continue,
            // A dead listener reports its condition on every call, so
            // the loop must leave rather than spin on it.
            Readiness::Gone => {
                warn!(log, "listener is unusable"; "device" => device);
                return;
            }
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => {
                reported = false;
                stream
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(error) => {
                if !reported {
                    debug!(log, "accept failed";
                        "device" => device, "error" => %error);
                    reported = true;
                }
                std::thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        if let Some(stream) = accepted_blocking(stream) {
            on_client(stream);
        }
    }
}
