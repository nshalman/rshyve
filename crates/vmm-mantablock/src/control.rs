// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The control thread: heartbeat, engine-loss detection, reattach and
//! Detach.
//!
//! A Pong does not show that the engine makes progress, so a request
//! older than the I/O limit also counts as a lost engine.

use std::sync::Arc;
use std::time::{Duration, Instant};

use manta_block_ring::control::Message;

use crate::conn::{Conn, Polled};
use crate::job::Job;
use crate::out::Lost;
use crate::queue::now_ns;
use crate::reattach::{self, Plan};
use crate::ring::Shared;
use crate::Config;

/// Longest wait before the thread looks at the halt flag again.
const TICK: Duration = Duration::from_millis(50);

/// Frames taken per tick, so an engine that floods the socket cannot keep
/// the thread from its timers and the halt flag.
const FRAMES_PER_TICK: usize = 64;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Timers {
    heartbeat: Duration,
    heartbeat_limit: Duration,
    io_limit: Duration,
    deadline: Duration,
}

impl From<&Config> for Timers {
    fn from(c: &Config) -> Self {
        Self {
            heartbeat: c.heartbeat,
            heartbeat_limit: c.heartbeat_limit,
            io_limit: c.io_limit,
            deadline: c.deadline,
        }
    }
}

pub(crate) fn run<J: Job>(
    shared: &Arc<Shared<J>>,
    mut conn: Conn,
    t: Timers,
    plan: &Plan,
) {
    // When the held I/O fails. A reattach that sees no completion keeps
    // the bound of the loss before it, so an engine that comes back and
    // never serves cannot hold I/O for ever.
    let mut until: Option<Instant> = None;
    loop {
        if let Err(why) = watch(shared, &mut conn, t) {
            shared.lose(why);
        }
        if !shared.is_held() || shared.halted() {
            break;
        }
        let now = Instant::now();
        let bound = match until {
            Some(u) if !shared.take_progress() => u,
            _ => now + plan.limit,
        };
        until = Some(bound);
        match reattach::run(shared, plan, conn, bound) {
            Some(c) => conn = c,
            None => return,
        }
    }
    if shared.halted() && !shared.is_lost() && !shared.is_held() {
        detach(shared, &mut conn, t);
    }
    // An engine that did not finish Detach ends the attachment itself
    // when the socket closes.
}

fn watch<J: Job>(
    shared: &Shared<J>,
    conn: &mut Conn,
    t: Timers,
) -> Result<(), Lost> {
    let mut last_pong = Instant::now();
    let mut next_ping = last_pong;
    while !shared.halted() && !shared.stopping() {
        let now = Instant::now();
        if now >= next_ping {
            conn.send(&Message::Ping, now + t.deadline)
                .map_err(|e| Lost::Control(format!("Ping: {e}")))?;
            next_ping = now + t.heartbeat;
        }
        if now.duration_since(last_pong) > t.heartbeat_limit {
            return Err(Lost::Heartbeat);
        }
        let io_limit = u64::try_from(t.io_limit.as_nanos()).unwrap_or(u64::MAX);
        if shared
            .oldest()
            .is_some_and(|o| now_ns().saturating_sub(o) > io_limit)
        {
            return Err(Lost::Stalled);
        }
        // Frames left over from a full budget are already read, and the
        // socket may stay quiet until the next tick.
        if !conn.frame_buffered() {
            let until = next_ping.min(now + TICK);
            conn.readable(until)
                .map_err(|e| Lost::Control(format!("poll: {e}")))?;
        }
        for _ in 0..FRAMES_PER_TICK {
            if shared.halted() || shared.stopping() {
                break;
            }
            match conn.try_recv() {
                Ok(Polled::Frame(f)) if !f.fds.is_empty() => {
                    return Err(Lost::Control(
                        "descriptors after attach".into(),
                    ))
                }
                Ok(Polled::Frame(f)) => match f.msg {
                    Message::Pong => last_pong = Instant::now(),
                    Message::Error { code, text } => {
                        return Err(Lost::Control(format!(
                            "engine error {code}: {text}"
                        )))
                    }
                    m => {
                        return Err(Lost::Control(format!("unexpected {m:?}")))
                    }
                },
                Ok(Polled::Empty) => break,
                Ok(Polled::Closed) => {
                    return Err(Lost::Control("socket closed".into()))
                }
                Err(e) => return Err(Lost::Control(e.to_string())),
            }
        }
    }
    Ok(())
}

/// The engine answers Detached only once no request is in it. The wait is
/// the control deadline, not the I/O limit: halt blocks VM stop, and what
/// the engine still holds fails anyway.
fn detach<J: Job>(shared: &Shared<J>, conn: &mut Conn, t: Timers) {
    let until = Instant::now() + t.deadline;
    if conn.send(&Message::Detach, until).is_err() {
        return;
    }
    loop {
        match conn.recv(until) {
            Ok(Some(f)) => match f.msg {
                Message::Detached => return,
                Message::Pong => {}
                m => {
                    slog::warn!(shared.log, "mantablock: detach answered";
                        "reply" => ?m);
                    return;
                }
            },
            Ok(None) | Err(_) => {
                slog::warn!(shared.log, "mantablock: no Detached");
                return;
            }
        }
    }
}
