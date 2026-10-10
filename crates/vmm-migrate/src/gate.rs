// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The cutover gate: a paused, drained source asks the orchestrator
//! whether to hand the guest over.
//!
//! The agent listens on a Unix socket. Once the guest is paused, every
//! in-flight request is complete and the disks are flushed, the source
//! connects and the two exchange one line each way:
//!
//! ```text
//! source -> agent   READY rshyve-gate/1
//! agent  -> source  GO | ABORT
//! source -> agent   ACK GO | ACK ABORT
//! ```
//!
//! After `ACK GO` the source does not resume the guest by itself,
//! unless it has not committed by the cutover deadline (see
//! [`crate::cutover`]). Any other answer, a closed socket or no answer
//! within the budget is an abort: the source resumes. The orchestrator writes its own record
//! before it sends `GO`, so `GO` without `ACK GO` is a state it must
//! read back from the source, not assume. See `docs/migration-gate.md`.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::codec::MigrateError;
use crate::wire::cancel_or;
use crate::{GateState, MigrationStatus, StopGate};

/// The version this source speaks, in its READY line.
pub const GATE_PROTOCOL: &str = "rshyve-gate/1";

/// The longest line either side sends, newline included.
pub const MAX_LINE: usize = 64;

/// The default wait for an answer.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(10);

/// The longest wait an owner may ask for. The destination holds a read
/// deadline of its own across the gate, and a guest paused longer is an
/// outage, not a migration.
pub const MAX_BUDGET: Duration = Duration::from_secs(25);

/// What the agent answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Go,
    Abort(String),
}

/// Ask the agent at `path`. `Ok` only for `GO`; the gate state in
/// `status` is set to `Go` before the ACK goes out, so from then on the
/// owner must not roll the source back.
pub fn wait(
    path: &Path,
    budget: Duration,
    stop: &dyn StopGate,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    set(status, GateState::Waiting);
    let until = Instant::now() + budget.min(MAX_BUDGET);
    // A listener with a full backlog must not hold a paused guest past
    // the budget.
    let stream = crate::connect::unix_until(path, until, || stop.cancelled())?
        .map_err(|e| abort(status, format!("gate connect: {e}")))?;
    let shut = stream
        .try_clone()
        .map_err(|e| abort(status, format!("gate socket: {e}")))?;
    let shut_log = log.clone();
    stop.on_stop(Box::new(move || {
        if let Err(e) = shut.shutdown(Shutdown::Both) {
            slog::debug!(shut_log, "the gate was already closed";
                "error" => %e);
        }
    }));
    slog::info!(log, "waiting at the cutover gate";
        "gate" => %path.display(), "budget_ms" => budget.as_millis());
    match ask(stream, until, status, stop) {
        Ok(Ok(())) => {
            slog::info!(log, "cutover gate: go");
            Ok(())
        }
        Ok(Err(e)) => {
            slog::warn!(log, "cutover gate: go taken, its ACK failed";
                "error" => %e);
            Ok(())
        }
        Err(why) => {
            let error = abort(status, format!("cutover gate: {why}"));
            Err(cancel_or(stop, error))
        }
    }
}

/// One exchange. On `GO` the gate state is `Go` before the ACK is
/// written; the inner result is the ACK's.
fn ask(
    mut stream: UnixStream,
    until: Instant,
    status: &Arc<Mutex<MigrationStatus>>,
    stop: &dyn StopGate,
) -> Result<io::Result<()>, String> {
    write_line(&mut stream, &format!("READY {GATE_PROTOCOL}"), until)
        .map_err(|e| format!("READY: {e}"))?;
    let verdict = match read_line(&mut stream, until) {
        Ok(line) => match parse(&line) {
            Verdict::Go if !take_go(status, stop) => {
                Verdict::Abort(crate::CANCELLED.into())
            }
            verdict => verdict,
        },
        Err(e) => Verdict::Abort(format!("no answer: {e}")),
    };
    match verdict {
        Verdict::Go => {
            // Go holds from here even if the ACK is lost: the agent
            // reads the gate state back when it has no ACK.
            Ok(write_line(&mut stream, "ACK GO", until))
        }
        Verdict::Abort(why) => {
            // The abort holds whether or not the ACK arrives.
            set(status, GateState::Abort);
            match write_line(&mut stream, "ACK ABORT", until) {
                Ok(()) => Err(why),
                Err(e) => Err(format!("{why} (ACK ABORT failed: {e})")),
            }
        }
    }
}

/// `GO` and `ABORT` only, exactly. Anything else aborts.
pub fn parse(line: &str) -> Verdict {
    match line {
        "GO" => Verdict::Go,
        "ABORT" => Verdict::Abort("the orchestrator said abort".into()),
        other => Verdict::Abort(format!(
            "unknown answer '{}'",
            crate::wire::peer_text(other)
        )),
    }
}

/// Record the go, unless a cancel came first.
///
/// The check and the record are one step under the status lock. An
/// owner's cancel takes the same lock to check for a go, so exactly
/// one of the two wins.
fn take_go(status: &Arc<Mutex<MigrationStatus>>, stop: &dyn StopGate) -> bool {
    let Ok(mut s) = status.lock() else {
        return false;
    };
    if stop.cancelled() {
        return false;
    }
    s.gate = GateState::Go;
    true
}

fn set(status: &Arc<Mutex<MigrationStatus>>, gate: GateState) {
    if let Ok(mut s) = status.lock() {
        s.gate = gate;
    }
}

fn abort(status: &Arc<Mutex<MigrationStatus>>, why: String) -> MigrateError {
    set(status, GateState::Abort);
    MigrateError::Phase(why)
}

fn left(until: Instant) -> io::Result<Duration> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::ErrorKind::TimedOut.into());
    }
    Ok(left)
}

fn write_line(
    stream: &mut UnixStream,
    line: &str,
    until: Instant,
) -> io::Result<()> {
    stream.set_write_timeout(Some(left(until)?))?;
    stream.write_all(format!("{line}\n").as_bytes())
}

/// One line, without its newline. A line longer than [`MAX_LINE`], one
/// that is not ASCII, or the end of the stream before a newline is an
/// error.
fn read_line(stream: &mut UnixStream, until: Instant) -> io::Result<String> {
    let mut line = Vec::with_capacity(MAX_LINE);
    let mut byte = [0u8; 1];
    loop {
        stream.set_read_timeout(Some(left(until)?))?;
        match stream.read(&mut byte) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                line.push(byte[0]);
                if line.len() >= MAX_LINE {
                    return Err(io::Error::other("the line is too long"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    String::from_utf8(line)
        .ok()
        .filter(|l| l.is_ascii())
        .ok_or_else(|| io::Error::other("the line is not ASCII"))
}

#[cfg(test)]
mod tests;
