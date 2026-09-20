// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The two per-connection tables: a host socket for each `ConnKey`, and
//! the threads that serve them.
//!
//! # Invariants
//!
//! A connection is named by its socket, not by its key. The guest
//! picks the key, so it can close a connection and open the same key
//! again while an unlocked write, or a reader thread that has not
//! woken yet, still holds the old one. Every caller that acts on a
//! connection it learned about earlier therefore names the socket it
//! served, and [`VsockShared::release_socket`] refuses a key another
//! connection has taken.
//!
//! The reader table bounds the thread count. The mux connection cap
//! cannot: a peer that connects and sends no CONNECT line holds a
//! thread the mux has never heard of.

use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::JoinHandle;

use slog::debug;
use vmm_core::unixsock::write_all_bounded;

use super::shared::{VsockShared, HOST_WRITE_TIMEOUT};
use crate::vsock::mux::MAX_CONNS;
use crate::vsock::packet::ConnKey;

impl VsockShared {
    /// Write payload to a connection's host socket.
    pub(super) fn write_host(&self, key: ConnKey, data: &[u8]) {
        let socket = self
            .sockets
            .lock()
            .expect("sockets lock")
            .get(&key)
            .cloned();
        let Some(stream) = socket else {
            // The mux still holds the connection, so the guest thinks
            // it is live and would read a stream with a hole in it.
            // Dropping the payload in silence is what leaves it there.
            self.mux.queue_host_close(key);
            self.deliver_rx();
            return;
        };
        // The table stays unlocked for the write. A slow peer must not
        // hold the other vCPUs, or the halt sweep, behind it.
        if let Err(error) = write_all_bounded(&stream, data, HOST_WRITE_TIMEOUT)
        {
            debug!(self.log, "virtio-vsock host write gave up";
                "error" => %error);
            // A dead peer and one that stopped reading for
            // HOST_WRITE_TIMEOUT are the same to the guest, and the
            // guest hears it here rather than when the reader wakes:
            // the reader can no longer tell for a socket this took out
            // of the table.
            if self.release_socket(key, &stream) {
                self.mux.queue_host_close(key);
                self.deliver_rx();
            }
        }
    }

    /// Take `key` out of the table and shut its socket down, so that
    /// this connection's reader wakes.
    ///
    /// Returns whether the entry was still this socket's, which is the
    /// caller's permission to speak for the connection. A key another
    /// connection has taken, or one already released, answers false:
    /// the guest picks the key and can reopen it while a write or a
    /// reader thread still holds the old socket.
    pub(super) fn release_socket(
        &self,
        key: ConnKey,
        stream: &Arc<UnixStream>,
    ) -> bool {
        let mut sockets = self.sockets.lock().expect("sockets lock");
        let ours = sockets
            .get(&key)
            .is_some_and(|held| Arc::ptr_eq(held, stream));
        if ours {
            sockets.remove(&key);
        }
        drop(sockets);
        // The peer may already be gone; there is nothing to do.
        let _ = stream.shutdown(Shutdown::Both);
        ours
    }

    /// Put a host socket in the table, ready to be written to.
    ///
    /// The caller hands the returned handle to the connection's reader
    /// thread, so the reader and the table name one socket and the
    /// reader can prove at exit that the connection is still its own.
    pub(super) fn register_socket(
        &self,
        key: ConnKey,
        stream: UnixStream,
    ) -> Arc<UnixStream> {
        // Belt and braces only. This bounds a write on the platforms
        // where it works, and reports success and does nothing on
        // illumos, which is why `write_all_bounded` polls instead.
        if let Err(error) = stream.set_write_timeout(Some(HOST_WRITE_TIMEOUT)) {
            debug!(self.log, "virtio-vsock write timeout not set";
                "error" => %error);
        }
        let stream = Arc::new(stream);
        self.sockets
            .lock()
            .expect("sockets lock")
            .insert(key, Arc::clone(&stream));
        stream
    }

    pub(super) fn drop_socket(&self, key: ConnKey) {
        if let Some(stream) =
            self.sockets.lock().expect("sockets lock").remove(&key)
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// Every key the table holds now, for the halt sweep.
    pub(super) fn socket_keys(&self) -> Vec<ConnKey> {
        self.sockets
            .lock()
            .expect("sockets lock")
            .keys()
            .copied()
            .collect()
    }

    /// Register a reader thread, dropping the handles whose threads
    /// have ended.
    ///
    /// The vector is otherwise only drained by halt, so a peer that
    /// connects and disconnects in a loop grows it, and every handle in
    /// it, for the life of the VM.
    pub(super) fn track_reader(&self, handle: JoinHandle<()>) {
        let mut readers = self.readers.lock().expect("reader lock");
        readers.retain(|h| !h.is_finished());
        readers.push(handle);
    }

    /// Whether another reader thread may start.
    pub(super) fn reader_slot_free(&self) -> bool {
        let mut readers = self.readers.lock().expect("reader lock");
        readers.retain(|h| !h.is_finished());
        readers.len() < MAX_CONNS
    }

    /// Take every reader handle, for the halt join.
    pub(super) fn take_readers(&self) -> Vec<JoinHandle<()>> {
        self.readers
            .lock()
            .expect("reader lock")
            .drain(..)
            .collect()
    }
}
