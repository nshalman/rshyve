// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The framing both ends share: one stream type, one deadline policy,
//! and one set of message expectations.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tungstenite::WebSocket;

use crate::codec::{Message, MigrateError};
use crate::{MigrationPhase, MigrationStatus};

/// How much of a peer-supplied string reaches a log or an error.
///
/// The peer chooses these bytes, so newlines and terminal escapes get
/// out of them, and the length is theirs too.
const MAX_PEER_TEXT: usize = 200;

/// A peer-supplied string, bounded and escaped, safe to log.
pub fn peer_text(s: &str) -> String {
    let mut out: String = s.escape_debug().take(MAX_PEER_TEXT).collect();
    if out.len() < s.escape_debug().count() {
        out.push_str("...");
    }
    out
}

/// A migration transport: a byte stream whose read deadline the
/// protocol moves as the phases change.
///
/// Read deadlines only. `SO_SNDTIMEO` reports success on illumos and
/// the write still blocks forever, so the owner frees a blocked send by
/// shutting the socket down.
pub trait Transport: Read + Write {
    fn set_read_deadline(&self, budget: Duration) -> std::io::Result<()>;
}

impl Transport for std::os::unix::net::UnixStream {
    fn set_read_deadline(&self, budget: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(budget))
    }
}

impl Transport for std::net::TcpStream {
    fn set_read_deadline(&self, budget: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(budget))
    }
}

/// How long each end waits on the peer, by migration phase.
///
/// Before the pause the guest is still running on the source, so a slow
/// peer costs nothing but time. After it the guest is stopped on both
/// hosts, and every second of waiting is guest downtime, so the budget
/// is short enough that a rollback still happens while the outage is
/// small.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    pub pre_pause: Duration,
    pub post_pause: Duration,
    /// How long the source waits for the ZFS barrier to report.
    pub zfs_barrier: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            pre_pause: Duration::from_secs(300),
            post_pause: Duration::from_secs(30),
            zfs_barrier: Duration::from_secs(120),
        }
    }
}

/// The stream plus its phase-aware deadline, so no read can block
/// forever with the guest paused.
pub struct Chan<S: Transport> {
    ws: WebSocket<S>,
    deadlines: Deadlines,
}

impl<S: Transport> Chan<S> {
    pub fn new(
        stream: S,
        role: tungstenite::protocol::Role,
        deadlines: Deadlines,
    ) -> Result<Self, MigrateError> {
        stream.set_read_deadline(deadlines.pre_pause).map_err(|e| {
            MigrateError::Io(format!("set migration read deadline: {e}"))
        })?;
        Ok(Self {
            ws: WebSocket::from_raw_socket(
                stream,
                role,
                Some(crate::limits::ws_config()),
            ),
            deadlines,
        })
    }

    /// Shorten the deadline for everything after the pause.
    pub fn enter_post_pause(&mut self) -> Result<(), MigrateError> {
        self.ws
            .get_ref()
            .set_read_deadline(self.deadlines.post_pause)
            .map_err(|e| {
                MigrateError::Io(format!("shorten read deadline: {e}"))
            })
    }

    pub fn deadlines(&self) -> Deadlines {
        self.deadlines
    }

    pub fn send(&mut self, msg: &Message) -> Result<(), MigrateError> {
        let data = match msg.encode() {
            tungstenite::Message::Binary(d) => d,
            _ => return Err(MigrateError::Codec("non-binary encode".into())),
        };
        self.ws
            .send(tungstenite::Message::Binary(data))
            .map_err(|e| MigrateError::WebSocket(e.to_string()))
    }

    /// Every send before this end's commit goes through here, so
    /// nothing goes out after a stop cancelled the migration.
    pub fn send_uncommitted(
        &mut self,
        gate: &dyn crate::StopGate,
        msg: &Message,
    ) -> Result<(), MigrateError> {
        not_cancelled(gate)?;
        self.send(msg)
    }

    pub fn recv(&mut self) -> Result<Message, MigrateError> {
        let frame = self
            .ws
            .read()
            .map_err(|e| MigrateError::WebSocket(e.to_string()))?;
        match frame {
            tungstenite::Message::Binary(d) => {
                Message::decode(tungstenite::Message::Binary(d))
            }
            tungstenite::Message::Close(_) => {
                Err(MigrateError::WebSocket("closed".into()))
            }
            _ => Err(MigrateError::Codec("non-binary frame".into())),
        }
    }

    pub fn expect_okay(&mut self) -> Result<(), MigrateError> {
        expect_okay(self.recv()?)
    }

    pub fn expect_serialized<T: for<'de> serde::Deserialize<'de>>(
        &mut self,
    ) -> Result<T, MigrateError> {
        Message::deserialize(&expect_serialized(self.recv()?)?)
    }

    pub fn expect_mem_done(&mut self) -> Result<(), MigrateError> {
        match self.recv()? {
            Message::MemDone => Ok(()),
            other => Err(unexpected("MemDone", &other)),
        }
    }

    /// Tell the peer why this end stopped.
    ///
    /// The other end rolls back when the migration aborts, and it needs
    /// the reason: a dropped connection alone does not say whether the
    /// state was rejected or the network failed.
    pub fn report_failure(&mut self, err: &MigrateError, log: &slog::Logger) {
        slog::error!(log, "migration failed"; "error" => %err);
        if let Err(send_err) = self.send(&Message::Error(err.clone())) {
            slog::warn!(log, "could not report the failure to the peer";
                "error" => %send_err);
        }
    }

    pub fn close(&mut self) {
        if let Err(e) = self.ws.close(None) {
            // The peer may already be gone, which is not a failure:
            // the protocol committed before the close.
            slog::trace!(slog::Logger::root(slog::Discard, slog::o!()),
                "websocket close"; "error" => %e);
        }
    }
}

pub fn unexpected(expected: &str, got: &Message) -> MigrateError {
    MigrateError::UnexpectedMessage {
        expected: expected.into(),
        got: got.name().into(),
    }
}

fn expect_okay(msg: Message) -> Result<(), MigrateError> {
    match msg {
        Message::Okay => Ok(()),
        Message::Error(e) => {
            Err(MigrateError::RemoteError(peer_text(&e.to_string())))
        }
        other => Err(unexpected("Okay", &other)),
    }
}

fn expect_serialized(msg: Message) -> Result<Vec<u8>, MigrateError> {
    match msg {
        Message::Serialized(d) => Ok(d),
        Message::Error(e) => {
            Err(MigrateError::RemoteError(peer_text(&e.to_string())))
        }
        other => Err(unexpected("Serialized", &other)),
    }
}

pub fn not_cancelled(gate: &dyn crate::StopGate) -> Result<(), MigrateError> {
    if gate.cancelled() {
        return Err(crate::cancelled());
    }
    Ok(())
}

/// A stop refuses the migration's own pause and resume. That is a
/// cancel, which the peer has to hear about.
pub fn cancel_or(
    gate: &dyn crate::StopGate,
    error: MigrateError,
) -> MigrateError {
    if gate.cancelled() {
        return crate::cancelled();
    }
    error
}

pub fn set_phase(status: &Arc<Mutex<MigrationStatus>>, phase: MigrationPhase) {
    if let Ok(mut s) = status.lock() {
        s.phase = phase;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_text_strips_control_bytes_and_bounds_length() {
        // A peer that can write newlines into slog can forge log lines.
        assert_eq!(peer_text("a\nb\x1b[2J"), "a\\nb\\u{1b}[2J");
        let long = "x".repeat(MAX_PEER_TEXT * 2);
        let out = peer_text(&long);
        assert_eq!(out.len(), MAX_PEER_TEXT + 3);
        assert!(out.ends_with("..."));
    }

    struct Cancelled;

    impl crate::StopGate for Cancelled {
        fn cancelled(&self) -> bool {
            true
        }

        fn commit(&self) -> Result<(), MigrateError> {
            Err(crate::cancelled())
        }

        fn on_stop(&self, _shut: Box<dyn FnOnce() + Send>) {}
    }

    #[test]
    fn nothing_uncommitted_goes_out_after_a_cancel() {
        let (a, b) = std::os::unix::net::UnixStream::pair().expect("pair");
        let deadlines = Deadlines {
            pre_pause: Duration::from_millis(200),
            ..Deadlines::default()
        };
        let mut ours =
            Chan::new(a, tungstenite::protocol::Role::Client, deadlines)
                .expect("ours");
        let mut peer =
            Chan::new(b, tungstenite::protocol::Role::Server, deadlines)
                .expect("peer");

        let error = ours
            .send_uncommitted(&Cancelled, &Message::Okay)
            .expect_err("cancelled");

        assert!(crate::is_cancel(&error), "{error}");
        drop(ours);
        assert!(peer.recv().is_err(), "the peer heard a send");
    }
}
